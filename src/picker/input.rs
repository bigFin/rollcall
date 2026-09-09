use std::thread;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{domain::RuntimeOwner, tmux};

use super::{
    ALL_HOSTS, Action, DashboardRow, DashboardSelection, InputMode, PREVIEW_CAPTURE_LINES,
    PickerApp, PreviewContent, RefreshEvent, SessionPreview,
    dashboard::row_group_key,
    view::text::{cached_preview, cached_preview_source},
};

impl PickerApp {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Action {
        match self.input_mode {
            InputMode::Browse => self.handle_browse_key(key),
            InputMode::Search => self.handle_search_key(key),
            InputMode::Hosts => self.handle_host_key(key),
            InputMode::Help => self.handle_help_key(key),
            InputMode::Preview => self.handle_preview_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Char('/') => {
                self.input_mode = InputMode::Search;
                self.status = None;
                Action::None
            }
            KeyCode::Char('h') => {
                self.input_mode = InputMode::Hosts;
                self.status = None;
                Action::None
            }
            KeyCode::Char('?') => {
                self.input_mode = InputMode::Help;
                self.status = None;
                Action::None
            }
            KeyCode::Char('i') => {
                self.show_details = !self.show_details;
                self.status = None;
                Action::None
            }
            KeyCode::Char('p') => {
                self.start_preview();
                Action::None
            }
            KeyCode::Tab | KeyCode::Char(' ') => {
                self.toggle_selected_row();
                Action::None
            }
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('a') => self.archive_action(false),
            KeyCode::Char('x') => self.acknowledge_action(),
            KeyCode::Char('0') => Action::SetHostFilter(ALL_HOSTS.to_owned()),
            KeyCode::Char('[') => self.cycle_host_filter(-1),
            KeyCode::Char(']') => self.cycle_host_filter(1),
            KeyCode::Char('j') | KeyCode::Down => {
                self.status = None;
                self.select_next();
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.status = None;
                self.select_previous();
                Action::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.status = None;
                self.select_first();
                Action::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.status = None;
                self.select_last();
                Action::None
            }
            KeyCode::Enter => self.selected_action(),
            _ => Action::None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.input_mode = InputMode::Browse;
                Action::None
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.rebuild_rows_selecting(None);
                Action::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.rebuild_rows_selecting(None);
                Action::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.rebuild_rows_selecting(None);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_host_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.input_mode = InputMode::Browse;
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.host_choices.is_empty() {
                    self.host_selected = (self.host_selected + 1).min(self.host_choices.len() - 1);
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.host_selected = self.host_selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Enter => self
                .host_choices
                .get(self.host_selected)
                .cloned()
                .map_or(Action::None, Action::SetHostFilter),
            _ => Action::None,
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') => {
                self.input_mode = InputMode::Browse;
            }
            _ => {}
        }
        Action::None
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'p') => {
                self.close_preview();
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down | KeyCode::PageDown => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = preview
                        .scroll_from_bottom
                        .saturating_sub(if key.code == KeyCode::PageDown { 10 } else { 1 });
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::PageUp => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = preview
                        .scroll_from_bottom
                        .saturating_add(if key.code == KeyCode::PageUp { 10 } else { 1 });
                }
                Action::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = usize::MAX;
                }
                Action::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = 0;
                }
                Action::None
            }
            KeyCode::Char('a') => {
                let Some(session_id) = self
                    .preview
                    .as_ref()
                    .map(|preview| preview.session_id.clone())
                else {
                    return Action::None;
                };
                let Some(archived) = self.archived_state_for_session(&session_id) else {
                    self.close_preview();
                    self.status =
                        Some("That session is no longer in the cached inventory.".to_owned());
                    return Action::None;
                };
                self.close_preview();
                Action::SetArchived {
                    session_id,
                    archived: !archived,
                    attach: false,
                }
            }
            KeyCode::Char('x') => self
                .preview
                .as_ref()
                .map(|preview| Action::Acknowledge(preview.session_id.clone()))
                .unwrap_or(Action::None),
            KeyCode::Enter => {
                let Some(session_id) = self
                    .preview
                    .as_ref()
                    .map(|preview| preview.session_id.clone())
                else {
                    return Action::None;
                };
                self.action_for_session(&session_id)
            }
            _ => Action::None,
        }
    }

    fn selected_action(&mut self) -> Action {
        if self.selected_section().is_some() || self.selected_group_key().is_some() {
            self.toggle_selected_row();
            return Action::None;
        }
        let Some(session_id) = self.selected_session().map(|session| session.id.clone()) else {
            return Action::None;
        };
        self.action_for_session(&session_id)
    }

    fn action_for_session(&mut self, session_id: &str) -> Action {
        let Some((session, settled)) = self
            .active
            .iter()
            .map(|session| (session, false))
            .chain(self.settled.iter().map(|session| (session, true)))
            .find(|(session, _)| session.id == session_id)
        else {
            self.close_preview();
            self.status = Some("That session is no longer in the cached inventory.".to_owned());
            return Action::None;
        };
        if session.runtime == RuntimeOwner::ExternalFrontend && self.fresh_ids.contains(&session.id)
        {
            let message =
                "Owned by an external frontend; close or release it before resuming here."
                    .to_owned();
            if let Some(preview) = self.preview.as_mut() {
                preview.notice = Some(message);
            } else {
                self.status = Some(message);
            }
            return Action::None;
        }

        if settled {
            Action::SetArchived {
                session_id: session.id.clone(),
                archived: false,
                attach: true,
            }
        } else {
            Action::Attach(session.id.clone())
        }
    }

    pub(super) fn archived_state_for_session(&self, session_id: &str) -> Option<bool> {
        self.active
            .iter()
            .any(|session| session.id == session_id)
            .then_some(false)
            .or_else(|| {
                self.settled
                    .iter()
                    .any(|session| session.id == session_id)
                    .then_some(true)
            })
    }

    pub(super) fn start_preview(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        self.preview_request_id = self.preview_request_id.wrapping_add(1);
        let request_id = self.preview_request_id;
        let fallback = cached_preview(&session);
        self.status = None;
        self.input_mode = InputMode::Preview;

        let can_capture = self.fresh_ids.contains(&session.id)
            && session.runtime == RuntimeOwner::TmuxFrontend
            && session.tmux.is_some();
        let content = if can_capture {
            PreviewContent::Loading
        } else {
            PreviewContent::Ready {
                source: cached_preview_source(&session),
                body: fallback.clone(),
            }
        };
        self.preview = Some(SessionPreview {
            request_id,
            session_id: session.id.clone(),
            title: session.title.clone(),
            host: session.host.clone(),
            scroll_from_bottom: 0,
            notice: None,
            content,
        });

        if let Some(binding) = session.tmux {
            if !can_capture {
                return;
            }
            let sender = self.refresh_tx.clone();
            let host = session.host;
            let session_id = session.id;
            thread::spawn(move || {
                let result = tmux::capture_pane(
                    &host,
                    &binding.session,
                    &binding.pane,
                    PREVIEW_CAPTURE_LINES,
                )
                .map_err(|error| error.to_string());
                let _ = sender.send(RefreshEvent::Preview {
                    request_id,
                    session_id,
                    result,
                });
            });
        }
    }

    fn close_preview(&mut self) {
        self.preview = None;
        self.input_mode = InputMode::Browse;
    }

    fn archive_action(&self, attach: bool) -> Action {
        self.selected_session()
            .and_then(|session| {
                self.archived_state_for_session(&session.id)
                    .map(|archived| (session, archived))
            })
            .map_or(Action::None, |(session, archived)| Action::SetArchived {
                session_id: session.id.clone(),
                archived: !archived,
                attach,
            })
    }

    fn acknowledge_action(&self) -> Action {
        self.selected_session()
            .map(|session| Action::Acknowledge(session.id.clone()))
            .unwrap_or(Action::None)
    }

    fn toggle_selected_row(&mut self) {
        let Some(row) = self.rows.get(self.selected_row) else {
            return;
        };
        let selection = match row {
            DashboardRow::Section { section, .. } => DashboardSelection::Section(*section),
            DashboardRow::Host { .. } | DashboardRow::Group { .. } => {
                let Some(group) = row_group_key(row) else {
                    return;
                };
                DashboardSelection::Group(group)
            }
            DashboardRow::Session(_) => {
                let group = self.rows[..=self.selected_row]
                    .iter()
                    .rev()
                    .find_map(row_group_key);
                let Some(group) = group else {
                    return;
                };
                DashboardSelection::Group(group)
            }
        };

        match &selection {
            DashboardSelection::Section(section) => {
                if !self.expanded_sections.remove(section) {
                    self.expanded_sections.insert(*section);
                }
            }
            DashboardSelection::Group(group) => {
                if !self.collapsed_groups.remove(group) {
                    self.collapsed_groups.insert(group.clone());
                }
            }
            DashboardSelection::Session(_) => return,
        }
        self.status = None;
        self.rebuild_rows_selecting(Some(selection));
    }

    pub(super) fn set_host_filter(&mut self, host: String) {
        self.host_filter = host;
        self.host_selected = self
            .host_choices
            .iter()
            .position(|candidate| candidate == &self.host_filter)
            .unwrap_or_default();
        self.input_mode = InputMode::Browse;
        self.status = None;
        if self.host_filter != ALL_HOSTS
            && let Some(target) = self.target_for_observed_host(&self.host_filter).cloned()
        {
            self.request_host_now(&target);
        }
        self.rebuild_rows();
    }
    fn cycle_host_filter(&mut self, direction: isize) -> Action {
        if self.host_choices.is_empty() {
            return Action::None;
        }
        let current = self
            .host_choices
            .iter()
            .position(|host| host == &self.host_filter)
            .unwrap_or_default();
        let length = self.host_choices.len();
        let next = if direction.is_negative() {
            current.saturating_sub(direction.unsigned_abs())
        } else {
            current.saturating_add(direction as usize)
        }
        .min(length.saturating_sub(1));
        self.host_choices
            .get(next)
            .cloned()
            .map_or(Action::None, Action::SetHostFilter)
    }
}
