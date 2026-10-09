//! Bounded, generation-scoped delivery for background TUI work.
use crate::app::Action;
use parking_lot::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;

#[derive(Default)]
struct Scope {
    generation: AtomicU64,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    manager: Option<Arc<crate::mihomo_manager::ManagerInner>>,
}

pub(super) struct EventSender {
    sender: mpsc::Sender<Action>,
    scope: Arc<Scope>,
    generation: u64,
    local: Option<Arc<Mutex<super::handlers::LocalActionQueue>>>,
    core_generation: Option<u64>,
}

impl Clone for EventSender {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            scope: self.scope.clone(),
            generation: self.generation,
            local: None,
            core_generation: self.core_generation,
        }
    }
}

impl From<mpsc::Sender<Action>> for EventSender {
    fn from(sender: mpsc::Sender<Action>) -> Self {
        Self {
            sender,
            scope: Arc::new(Scope::default()),
            generation: 0,
            local: None,
            core_generation: None,
        }
    }
}

impl EventSender {
    pub(super) fn new(sender: mpsc::Sender<Action>) -> Self {
        let result = Self::from(sender);
        result.scope.generation.store(1, Ordering::SeqCst);
        Self {
            generation: 1,
            ..result
        }
    }

    pub(super) fn with_manager(mut self, manager: Arc<crate::mihomo_manager::ManagerInner>) -> Self {
        let generation = manager.generation.load(Ordering::SeqCst);
        Arc::get_mut(&mut self.scope)
            .expect("scope is uniquely owned during construction")
            .manager = Some(manager);
        self.core_generation = Some(generation);
        self
    }

    pub(super) fn with_local(mut self, local: Arc<Mutex<super::handlers::LocalActionQueue>>) -> Self {
        self.local = Some(local);
        self
    }

    pub(super) fn for_current(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            scope: self.scope.clone(),
            generation: self.current_generation(),
            local: None,
            core_generation: self.scope.manager.as_ref().map(|manager| {
                if manager.restarting.load(Ordering::SeqCst) {
                    u64::MAX
                } else {
                    manager.generation.load(Ordering::SeqCst)
                }
            }),
        }
    }

    /// Guided core results survive ordinary stream cancellation. Their own
    /// operation identity and cancellation token are checked by the reducer.
    pub(super) fn for_operation(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            scope: self.scope.clone(),
            generation: 0,
            local: None,
            core_generation: None,
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn current_generation(&self) -> u64 {
        self.scope.generation.load(Ordering::SeqCst)
    }

    pub(super) async fn send(&self, action: Action) -> Result<(), ()> {
        if let Some(local) = &self.local {
            return local.lock().push(action).map_err(|_| ());
        }
        if self.generation != 0 && self.generation != self.current_generation() {
            return Err(());
        }
        let permit = self.sender.reserve().await.map_err(|_| ())?;
        if self.generation != 0 && self.generation != self.current_generation() {
            return Err(());
        }
        permit.send(if self.generation == 0 {
            action
        } else {
            Action::Background {
                generation: self.generation,
                core_generation: self.core_generation,
                action: Box::new(action),
            }
        });
        Ok(())
    }

    pub(super) fn spawn<F>(&self, work: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let task = tokio::spawn(work);
        let mut tasks = self.scope.tasks.lock();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task.abort_handle());
        task
    }

    pub(super) fn cancel(&self) {
        self.scope.generation.fetch_add(1, Ordering::SeqCst);
        for task in self.scope.tasks.lock().drain(..) {
            task.abort();
        }
    }

    pub(super) async fn cancel_and_wait(&self) {
        self.scope.generation.fetch_add(1, Ordering::SeqCst);
        let tasks: Vec<_> = self.scope.tasks.lock().drain(..).collect();
        for task in &tasks {
            task.abort();
        }
        // Await destruction of owned futures, including transaction guards,
        // before a final stop can race their rollback.
        while tasks.iter().any(|task| !task.is_finished()) {
            tokio::task::yield_now().await;
        }
    }

    pub(super) fn accept(&self, action: Action) -> Option<Action> {
        match action {
            Action::Background {
                generation,
                core_generation,
                action,
            } => {
                if generation != self.current_generation() {
                    return None;
                }
                if is_core_snapshot(&action)
                    && core_generation
                        != self
                            .scope
                            .manager
                            .as_ref()
                            .map(|manager| manager.generation.load(Ordering::SeqCst))
                {
                    return None;
                }
                Some(*action)
            }
            other => Some(other),
        }
    }
}

fn is_core_snapshot(action: &Action) -> bool {
    matches!(
        action,
        Action::ProxiesFetched(_)
            | Action::ProxiesFailed(_)
            | Action::ProxyDelayKeysFetched(_)
            | Action::ConnectionsFetched(_)
            | Action::ConnectionsFailed(_)
            | Action::TrafficFailed(_)
            | Action::LogsFailed(_)
            | Action::RulesFetched(_)
            | Action::RulesFailed(_)
            | Action::RuleProvidersFetched(_)
            | Action::RuleProvidersFailed(_)
            | Action::DelayResult(..)
            | Action::DelayFailed(..)
            | Action::BatchDelayResult(..)
            | Action::BatchDelayFailed(..)
            | Action::BatchDelayResolved(_)
            | Action::SysProxyReassert
            | Action::ModeChanged { announce: false, .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reservation_keeps_control_order_under_pressure() {
        let (tx, mut rx) = mpsc::channel(1);
        let tx = EventSender::new(tx).clone();
        tx.send(Action::ProxiesRefresh).await.unwrap();
        let next = tx.clone();
        let pending = tokio::spawn(async move { next.send(Action::ConnectionsRefresh).await });
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        assert!(matches!(
            tx.accept(rx.recv().await.unwrap()),
            Some(Action::ProxiesRefresh)
        ));
        pending.await.unwrap().unwrap();
        assert!(matches!(
            tx.accept(rx.recv().await.unwrap()),
            Some(Action::ConnectionsRefresh)
        ));
    }
    #[tokio::test]
    async fn cancelled_generation_cannot_publish_or_resurrect_queued_ready() {
        let (tx, mut rx) = mpsc::channel(2);
        let tx = EventSender::new(tx);
        let old = tx.clone();
        old.send(Action::CoreStarted {
            version: None,
            binary_path: None,
            binary_source: None,
        })
        .await
        .unwrap();
        tx.cancel();
        assert!(tx.accept(rx.recv().await.unwrap()).is_none());
        assert!(old.send(Action::CoreExited(0)).await.is_err());
        tx.for_current().send(Action::ProxiesRefresh).await.unwrap();
        assert!(matches!(
            tx.accept(rx.recv().await.unwrap()),
            Some(Action::ProxiesRefresh)
        ));
    }
    #[tokio::test]
    async fn cancel_aborts_owned_background_work() {
        let (tx, _rx) = mpsc::channel(1);
        let tx = EventSender::new(tx);
        let handle = tx.spawn(std::future::pending::<()>());
        tx.cancel();
        assert!(handle.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn final_cancellation_waits_for_transaction_cleanup_and_filters_old_core_snapshots() {
        let (sender, mut receiver) = mpsc::channel(2);
        let manager = Arc::new(crate::mihomo_manager::ManagerInner::new());
        manager.generation.store(1, Ordering::SeqCst);
        let tx = EventSender::new(sender).with_manager(manager.clone());
        tx.send(Action::ProxiesFetched(std::collections::HashMap::new()))
            .await
            .unwrap();
        manager.generation.store(2, Ordering::SeqCst);
        assert!(tx.accept(receiver.recv().await.unwrap()).is_none());

        struct Cleanup(Arc<AtomicU64>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let cleaned = Arc::new(AtomicU64::new(0));
        let counter = cleaned.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let work = tx.spawn(async move {
            let _cleanup = Cleanup(counter);
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        tx.cancel_and_wait().await;
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert!(work.await.unwrap_err().is_cancelled());
    }
}
