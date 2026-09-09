use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    agents,
    domain::{Activity, Session},
    reconnect::{HostPhase, HostReconnectState},
};

use super::{
    ALL_HOSTS, DAY_SECONDS, DashboardGroupKey, DashboardRow, DashboardSection, DashboardSelection,
    GroupConnectivity, PickerApp, SessionRef, WEEK_SECONDS, view::text::activity_label,
};

impl PickerApp {
    pub(super) fn rebuild_host_choices(&mut self) {
        let mut hosts = BTreeSet::new();
        hosts.insert(ALL_HOSTS.to_owned());
        hosts.extend(
            self.discovery_targets
                .iter()
                .map(|target| agents::observed_host(target)),
        );
        hosts.extend(
            self.active
                .iter()
                .chain(&self.settled)
                .map(|session| session.host.clone()),
        );
        self.host_choices = hosts.into_iter().collect();
        self.host_choices
            .sort_by_key(|host| if host == ALL_HOSTS { 0 } else { 1 });
        if !self
            .host_choices
            .iter()
            .any(|host| host == &self.host_filter)
        {
            self.host_filter = ALL_HOSTS.to_owned();
        }
        self.host_selected = self
            .host_choices
            .iter()
            .position(|host| host == &self.host_filter)
            .unwrap_or_default();
    }

    pub(super) fn rebuild_rows(&mut self) {
        let selection = self.selected_selection();
        self.rebuild_rows_selecting(selection);
    }

    pub(super) fn rebuild_rows_selecting(&mut self, selection: Option<DashboardSelection>) {
        let query = self.query.to_lowercase();
        let query_active = !query.is_empty();
        let now = unix_now_seconds();
        let mut sections: BTreeMap<DashboardSection, Vec<SessionRef>> = BTreeMap::new();

        for (index, session) in self.active.iter().enumerate() {
            if (self.host_filter == ALL_HOSTS || session.host == self.host_filter)
                && session_matches(session, &query)
            {
                sections
                    .entry(dashboard_section(
                        session,
                        false,
                        self.unread_ids.contains(&session.id),
                        now,
                    ))
                    .or_default()
                    .push(SessionRef::Active(index));
            }
        }
        for (index, session) in self.settled.iter().enumerate() {
            if (self.host_filter == ALL_HOSTS || session.host == self.host_filter)
                && session_matches(session, &query)
            {
                sections
                    .entry(dashboard_section(
                        session,
                        true,
                        self.unread_ids.contains(&session.id),
                        now,
                    ))
                    .or_default()
                    .push(SessionRef::Settled(index));
            }
        }

        let mut rows = Vec::new();
        for section in DashboardSection::ALL {
            let sessions = sections.remove(&section).unwrap_or_default();
            let section_expanded =
                self.expanded_sections.contains(&section) || (query_active && !sessions.is_empty());
            rows.push(DashboardRow::Section {
                section,
                count: sessions.len(),
                expanded: section_expanded,
            });
            if !section_expanded {
                continue;
            }

            let mut hosts: BTreeMap<String, Vec<SessionRef>> = BTreeMap::new();
            for session_ref in sessions {
                hosts
                    .entry(self.session_for_ref(session_ref).host.clone())
                    .or_default()
                    .push(session_ref);
            }
            let mut hosts = hosts.into_iter().collect::<Vec<_>>();
            hosts.sort_by(|left, right| {
                self.compare_session_groups(&left.1, &right.1)
                    .then(left.0.cmp(&right.0))
            });

            for (host, sessions) in hosts {
                let fresh = sessions.iter().any(|session_ref| {
                    self.fresh_ids
                        .contains(&self.session_for_ref(*session_ref).id)
                });
                let host_key = DashboardGroupKey::Host {
                    section,
                    host: host.clone(),
                };
                let host_expanded = query_active || !self.collapsed_groups.contains(&host_key);
                rows.push(DashboardRow::Host {
                    section,
                    host: host.clone(),
                    count: sessions.len(),
                    fresh,
                    expanded: host_expanded,
                });
                if !host_expanded {
                    continue;
                }

                let mut projects: BTreeMap<String, Vec<SessionRef>> = BTreeMap::new();
                for session_ref in sessions {
                    projects
                        .entry(self.session_for_ref(session_ref).cwd.clone())
                        .or_default()
                        .push(session_ref);
                }
                let mut projects = projects.into_iter().collect::<Vec<_>>();
                projects.sort_by(|left, right| {
                    self.compare_session_groups(&left.1, &right.1)
                        .then(left.0.cmp(&right.0))
                });

                for (cwd, mut sessions) in projects {
                    sessions.sort_by_key(|session_ref| {
                        let session = self.session_for_ref(*session_ref);
                        (
                            !self.unread_ids.contains(&session.id),
                            activity_priority(session.activity),
                            std::cmp::Reverse(session.last_interaction_unix_seconds),
                        )
                    });
                    let fresh = sessions.iter().any(|session_ref| {
                        self.fresh_ids
                            .contains(&self.session_for_ref(*session_ref).id)
                    });
                    let project_key = DashboardGroupKey::Project {
                        section,
                        host: host.clone(),
                        cwd: cwd.clone(),
                    };
                    let project_expanded =
                        query_active || !self.collapsed_groups.contains(&project_key);
                    rows.push(DashboardRow::Group {
                        section,
                        cwd,
                        host: host.clone(),
                        count: sessions.len(),
                        fresh,
                        expanded: project_expanded,
                    });
                    if project_expanded {
                        rows.extend(sessions.into_iter().map(DashboardRow::Session));
                    }
                }
            }
        }

        self.rows = rows;
        self.selected_row = selection
            .and_then(|selection| {
                self.rows.iter().position(|row| match &selection {
                    DashboardSelection::Section(section) => {
                        matches!(row, DashboardRow::Section { section: candidate, .. } if candidate == section)
                    }
                    DashboardSelection::Group(group) => row_group_key(row).as_ref() == Some(group),
                    DashboardSelection::Session(selected_id) => {
                        matches!(row, DashboardRow::Session(session_ref) if self.session_for_ref(*session_ref).id == *selected_id)
                    }
                })
            })
            .or_else(|| {
                self.rows
                    .iter()
                    .position(|row| matches!(row, DashboardRow::Session(_)))
            })
            .or_else(|| self.rows.iter().position(is_navigable_row))
            .unwrap_or_default();
    }

    fn compare_session_groups(
        &self,
        left: &[SessionRef],
        right: &[SessionRef],
    ) -> std::cmp::Ordering {
        let left_unread = left.iter().any(|session_ref| {
            self.unread_ids
                .contains(&self.session_for_ref(*session_ref).id)
        });
        let right_unread = right.iter().any(|session_ref| {
            self.unread_ids
                .contains(&self.session_for_ref(*session_ref).id)
        });
        let left_priority = left
            .iter()
            .map(|session_ref| activity_priority(self.session_for_ref(*session_ref).activity))
            .min()
            .unwrap_or(u8::MAX);
        let right_priority = right
            .iter()
            .map(|session_ref| activity_priority(self.session_for_ref(*session_ref).activity))
            .min()
            .unwrap_or(u8::MAX);
        let left_recency = left
            .iter()
            .map(|session_ref| {
                self.session_for_ref(*session_ref)
                    .last_interaction_unix_seconds
            })
            .max()
            .unwrap_or_default();
        let right_recency = right
            .iter()
            .map(|session_ref| {
                self.session_for_ref(*session_ref)
                    .last_interaction_unix_seconds
            })
            .max()
            .unwrap_or_default();
        right_unread
            .cmp(&left_unread)
            .then_with(|| left_priority.cmp(&right_priority))
            .then_with(|| right_recency.cmp(&left_recency))
    }

    pub(super) fn session_for_ref(&self, session_ref: SessionRef) -> &Session {
        match session_ref {
            SessionRef::Active(index) => &self.active[index],
            SessionRef::Settled(index) => &self.settled[index],
        }
    }

    pub(super) fn selected_section(&self) -> Option<DashboardSection> {
        match self.rows.get(self.selected_row) {
            Some(DashboardRow::Section { section, .. }) => Some(*section),
            _ => None,
        }
    }
    pub(super) fn selected_selection(&self) -> Option<DashboardSelection> {
        match self.rows.get(self.selected_row) {
            Some(DashboardRow::Section { section, .. }) => {
                Some(DashboardSelection::Section(*section))
            }
            Some(row @ (DashboardRow::Host { .. } | DashboardRow::Group { .. })) => {
                row_group_key(row).map(DashboardSelection::Group)
            }
            Some(DashboardRow::Session(session_ref)) => Some(DashboardSelection::Session(
                self.session_for_ref(*session_ref).id.clone(),
            )),
            None => None,
        }
    }

    pub(super) fn selected_group_key(&self) -> Option<DashboardGroupKey> {
        self.rows.get(self.selected_row).and_then(row_group_key)
    }

    pub(super) fn selected_session(&self) -> Option<&Session> {
        match self.rows.get(self.selected_row) {
            Some(DashboardRow::Session(session_ref)) => Some(self.session_for_ref(*session_ref)),
            _ => None,
        }
    }

    pub(super) fn select_first(&mut self) {
        self.selected_row = self
            .rows
            .iter()
            .position(|row| matches!(row, DashboardRow::Session(_)))
            .or_else(|| self.rows.iter().position(is_navigable_row))
            .unwrap_or_default();
    }

    pub(super) fn select_last(&mut self) {
        self.selected_row = self
            .rows
            .iter()
            .rposition(|row| matches!(row, DashboardRow::Session(_)))
            .or_else(|| self.rows.iter().rposition(is_navigable_row))
            .unwrap_or_default();
    }

    pub(super) fn select_next(&mut self) {
        if let Some(index) = self
            .rows
            .iter()
            .enumerate()
            .skip(self.selected_row.saturating_add(1))
            .find_map(|(index, row)| is_navigable_row(row).then_some(index))
        {
            self.selected_row = index;
        }
    }

    pub(super) fn select_previous(&mut self) {
        if let Some(index) = self.rows[..self.selected_row.min(self.rows.len())]
            .iter()
            .rposition(is_navigable_row)
        {
            self.selected_row = index;
        }
    }

    pub(super) fn visible_session_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, DashboardRow::Session(_)))
            .count()
    }

    pub(super) fn section_counts(&self) -> BTreeMap<DashboardSection, usize> {
        let now = unix_now_seconds();
        let mut counts = BTreeMap::new();
        for session in &self.active {
            if self.host_filter == ALL_HOSTS || session.host == self.host_filter {
                *counts
                    .entry(dashboard_section(
                        session,
                        false,
                        self.unread_ids.contains(&session.id),
                        now,
                    ))
                    .or_insert(0) += 1;
            }
        }
        for session in &self.settled {
            if self.host_filter == ALL_HOSTS || session.host == self.host_filter {
                *counts
                    .entry(dashboard_section(
                        session,
                        true,
                        self.unread_ids.contains(&session.id),
                        now,
                    ))
                    .or_insert(0) += 1;
            }
        }
        counts
    }

    pub(super) fn group_connectivity(&self, host: &str, fresh: bool) -> GroupConnectivity {
        if fresh {
            return GroupConnectivity::Online;
        }
        let Some(target) = self.target_for_observed_host(host) else {
            return GroupConnectivity::Cached;
        };
        match self.reconnect.get(target).map(HostReconnectState::phase) {
            Some(HostPhase::Connecting) => GroupConnectivity::Checking,
            Some(HostPhase::Backoff) => GroupConnectivity::Offline(
                self.reconnect
                    .get(target)
                    .and_then(|state| state.retry_in(Instant::now()))
                    .map(|duration| duration.as_secs().max(1)),
            ),
            Some(HostPhase::Blocked) => GroupConnectivity::Blocked,
            Some(HostPhase::Online | HostPhase::Cached) | None => GroupConnectivity::Cached,
        }
    }
}

pub(super) fn row_group_key(row: &DashboardRow) -> Option<DashboardGroupKey> {
    match row {
        DashboardRow::Host { section, host, .. } => Some(DashboardGroupKey::Host {
            section: *section,
            host: host.clone(),
        }),
        DashboardRow::Group {
            section, host, cwd, ..
        } => Some(DashboardGroupKey::Project {
            section: *section,
            host: host.clone(),
            cwd: cwd.clone(),
        }),
        _ => None,
    }
}

pub(super) fn is_navigable_row(row: &DashboardRow) -> bool {
    matches!(
        row,
        DashboardRow::Section { .. }
            | DashboardRow::Host { .. }
            | DashboardRow::Group { .. }
            | DashboardRow::Session(_)
    )
}

pub(super) fn unix_now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(super) fn dashboard_section(
    session: &Session,
    settled: bool,
    unread: bool,
    now: u64,
) -> DashboardSection {
    if settled {
        return DashboardSection::Archive;
    }
    if activity_is_current(session.activity) || unread {
        return DashboardSection::Current;
    }
    let age = now.saturating_sub(session.last_interaction_unix_seconds);
    if age <= DAY_SECONDS {
        DashboardSection::LastDay
    } else if age <= WEEK_SECONDS {
        DashboardSection::LastWeek
    } else {
        DashboardSection::Archive
    }
}

fn activity_is_current(activity: Activity) -> bool {
    matches!(
        activity,
        Activity::Working | Activity::WaitingApproval | Activity::WaitingInput | Activity::Failed
    )
}

pub(super) fn session_matches(session: &Session, query: &str) -> bool {
    query.is_empty()
        || [
            session.title.as_str(),
            session.last_message.as_str(),
            session.cwd.as_str(),
            session.host.as_str(),
            session.source.as_str(),
            activity_label(session.activity),
            session.native_session_id.as_str(),
            session
                .tmux
                .as_ref()
                .map_or("", |binding| binding.session.as_str()),
        ]
        .iter()
        .any(|value| value.to_lowercase().contains(query))
}

const fn activity_priority(activity: Activity) -> u8 {
    match activity {
        Activity::Working => 0,
        Activity::WaitingApproval | Activity::WaitingInput => 1,
        Activity::Failed => 2,
        Activity::Completed => 3,
        Activity::Unknown => 4,
    }
}
