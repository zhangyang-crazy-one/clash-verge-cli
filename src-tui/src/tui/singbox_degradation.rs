//! Surface what a Clash → sing-box conversion lost, in one line.
//!
//! The apply report only carries counts ("… 4 notes"), and the detail
//! lines go to the log, so a user running a converted Clash profile on
//! sing-box never learns that 10 mieru nodes were skipped or that
//! `dns.fallback-filter` was ignored — and nobody tells them that a native
//! sing-box subscription would avoid all of it. This module turns the
//! grouped [`crate::runtime_config::SingboxDegradationRecord`] into one
//! localized status-bar line (the TUI's "recent message" surface).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::app::App;
use crate::runtime_config::{DegradationCategory, SingboxDegradationDigest};

/// Seq of the record already shown. A newer apply supersedes it, so the
/// same losses are never repeated on every subsequent event.
static SHOWN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Localized category label with its count, e.g. "跳过 10 个节点".
fn category_fragment(app: &App, category: DegradationCategory, count: usize) -> String {
    let key = match category {
        DegradationCategory::Nodes => "singbox.degradation.nodes",
        DegradationCategory::Rules => "singbox.degradation.rules",
        DegradationCategory::Dns => "singbox.degradation.dns",
        DegradationCategory::Groups => "singbox.degradation.groups",
    };
    app.tr(key).replace("{count}", &count.to_string())
}

/// The full notice: headline, one fragment per non-empty category, then the
/// native-subscription hint.
pub(crate) fn notice_text(app: &App, digest: &SingboxDegradationDigest) -> String {
    let mut parts = vec![
        app.tr("singbox.degradation.notice")
            .replace("{count}", &digest.category_count().to_string()),
    ];
    for (category, count) in &digest.categories {
        if *count > 0 {
            parts.push(category_fragment(app, *category, *count));
        }
    }
    parts.push(app.tr("singbox.degradation.hint").to_string());
    parts.join(" · ")
}

/// Show the last apply's degradation digest if it has not been shown yet.
/// Called from the action handler after every event: the apply itself runs
/// in a background task and reports through the action channel, so this is
/// where its result becomes visible.
pub(crate) async fn maybe_surface(app: &mut App) {
    let Some(record) = crate::runtime_config::last_singbox_degradation().await else {
        return;
    };
    if record.seq <= SHOWN_SEQ.load(Ordering::SeqCst) {
        return;
    }
    // Claim the record before rendering: a notice is shown once per apply.
    if SHOWN_SEQ.fetch_max(record.seq, Ordering::SeqCst) >= record.seq {
        return;
    }
    app.status_msg = Some(notice_text(app, &record.digest));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn digest() -> SingboxDegradationDigest {
        let mut categories = BTreeMap::new();
        categories.insert(DegradationCategory::Nodes, 10);
        categories.insert(DegradationCategory::Rules, 5);
        categories.insert(DegradationCategory::Dns, 8);
        SingboxDegradationDigest {
            notes: 13,
            nodes_skipped: 10,
            fields_degraded: 0,
            categories,
        }
    }

    #[test]
    fn the_notice_groups_the_losses_and_hints_at_a_native_subscription() {
        let app = App::new();
        let english = notice_text(&app, &digest());
        assert_eq!(
            english,
            "sing-box conversion degraded (3 categories) \u{b7} 10 nodes skipped \u{b7} 5 rules ignored \
             \u{b7} 8 DNS fields degraded \u{b7} a native sing-box subscription avoids all of these losses"
        );
        let mut app = App::new();
        app.language = crate::i18n::Language::SimplifiedChinese;
        let chinese = notice_text(&app, &digest());
        assert_eq!(
            chinese,
            "sing-box \u{8f6c}\u{6362}\u{964d}\u{7ea7}\u{ff08}3 \u{7c7b}\u{ff09} \u{b7} \u{8df3}\u{8fc7} 10 \
             \u{4e2a}\u{8282}\u{70b9} \u{b7} \u{5ffd}\u{7565} 5 \u{6761}\u{89c4}\u{5219} \u{b7} 8 \u{9879} DNS \
             \u{5b57}\u{6bb5}\u{964d}\u{7ea7} \u{b7} \u{5efa}\u{8bae}\u{6539}\u{7528} sing-box \u{539f}\u{751f}\
             \u{8ba2}\u{9605}\u{ff0c}\u{53ef}\u{907f}\u{514d}\u{4ee5}\u{4e0a}\u{5168}\u{90e8}\u{635f}\u{5931}"
        );
        assert!(!chinese.contains("{count}"), "every placeholder must be filled");
    }

    #[tokio::test]
    async fn a_recorded_apply_is_surfaced_once_and_a_clean_apply_clears_it() {
        SHOWN_SEQ.store(0, Ordering::SeqCst);
        let mut app = App::new();
        // Nothing recorded yet: the status line is left alone.
        app.status_msg = Some("Profile switched".into());
        maybe_surface(&mut app).await;
        assert_eq!(app.status_msg.as_deref(), Some("Profile switched"));

        crate::runtime_config::record_singbox_degradation(&lossy_parts()).await;
        maybe_surface(&mut app).await;
        let shown = app.status_msg.clone().expect("the notice replaces the last message");
        assert!(shown.starts_with("sing-box conversion degraded"), "{shown}");

        // The same apply must not shout again on the next unrelated event.
        app.status_msg = Some("Proxies refreshed".into());
        maybe_surface(&mut app).await;
        assert_eq!(app.status_msg.as_deref(), Some("Proxies refreshed"));

        crate::runtime_config::record_singbox_degradation(&clean_parts()).await;
        maybe_surface(&mut app).await;
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Proxies refreshed"),
            "a lossless apply clears the warning instead of leaving it up"
        );
    }

    fn lossy_parts() -> crate::mihomo_manager::manager::SingboxParts {
        parts(
            &["edge: unsupported protocol mieru"],
            &[
                "dns.fallback-filter: ignored; no typed equivalent",
                "select: dropped duplicate member 'a'",
            ],
        )
    }

    fn clean_parts() -> crate::mihomo_manager::manager::SingboxParts {
        parts(&[], &[])
    }

    fn parts(skipped: &[&str], notes: &[&str]) -> crate::mihomo_manager::manager::SingboxParts {
        let owned = |items: &[&str]| items.iter().map(|item| (*item).to_string()).collect::<Vec<_>>();
        crate::mihomo_manager::manager::SingboxParts {
            conversion: crate::singbox::convert::ProfileConversion {
                outbounds: vec![],
                groups: vec![],
                skipped: owned(skipped),
                degraded: vec![],
                notes: owned(notes),
            },
            profile_used: true,
            route_rules: vec![],
            rule_sets: vec![],
            dns_section: None,
            default_domain_resolver: None,
        }
    }
}
