pub mod connections;
pub mod home;
pub mod logs;
pub mod profiles;
pub mod proxies;
pub mod rules;
pub mod settings;
pub mod unlock;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::{App, View};

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match app.view {
        View::Home => home::draw(frame, area, app),
        View::Profiles => profiles::draw(frame, area, app),
        View::Proxies => proxies::draw(frame, area, app),
        View::Connections => connections::draw(frame, area, app),
        View::Rules => rules::draw(frame, area, app),
        View::Logs => logs::draw(frame, area, app),
        View::Unlock => unlock::draw(frame, area, app),
        View::Settings => settings::draw(frame, area, app),
    }
}
