//! Watcher views drawn inside other screens (the chat sidebar, the feature detail) run their
//! actions through the Watchers screen, which owns their requests.
use super::Shell;
use crate::screen::Cx;
use crate::watchers_screen::WatchAction;

impl Shell {
    /// Keeps the project names watcher rows attribute to.
    pub(super) fn sync_watch_projects(&mut self) {
        self.watchers.project_names = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().to_owned(), p.name.0.clone()))
            .collect();
    }

    pub(super) fn watch_act(&mut self, actions: Vec<WatchAction>) {
        if actions.is_empty() {
            return;
        }
        let online = self.online();
        let mut cx = Cx::new(self.client.as_mut(), online, &mut self.status, self.route);
        self.watchers.act(&mut cx, actions);
        let effects = cx.into_effects();
        self.run_effects(effects);
    }
}
