use std::{
    collections::BTreeMap,
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    agents,
    domain::{Activity, LiveObservation, RuntimeOwner, Session, merge_observed_activity},
    hosts::{self, HostDiscoveryError, SshHost},
    notify,
    reconnect::classify_failure,
    store::{DEFAULT_STALE_AFTER_SECONDS, SessionTransition},
};

use super::{
    ALL_HOSTS, DashboardSelection, LOCAL_ACTIVITY_POLL_INTERVAL, MAX_CONCURRENT_HOST_REFRESHES,
    NOTICE_DURATION, PickerApp, PickerError, PreviewContent, REMOTE_ACTIVITY_POLL_INTERVAL,
    RefreshEvent, view::text::cached_preview,
};

impl PickerApp {
    pub(super) fn start_refresh(&mut self) {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        self.enriching_hosts.clear();
        self.activity_pending.clear();
        self.pending_hosts.clear();
        self.last_activity_poll.clear();
        let now = Instant::now();
        for state in self.reconnect.values_mut() {
            state.request_now(now);
        }
        self.status = Some("Refreshing configured hosts…".to_owned());
        self.maybe_start_host_refreshes();
    }

    pub(super) fn request_host_now(&mut self, target: &str) {
        if self.pending_hosts.contains(target) || self.enriching_hosts.contains(target) {
            return;
        }
        if let Some(state) = self.reconnect.get_mut(target) {
            state.request_now(Instant::now());
        }
    }

    pub(super) fn maybe_start_host_refreshes(&mut self) {
        let capacity = MAX_CONCURRENT_HOST_REFRESHES.saturating_sub(self.pending_hosts.len());
        if capacity == 0 {
            return;
        }

        let now = Instant::now();
        let mut due = self
            .discovery_targets
            .iter()
            .filter(|target| {
                !self.pending_hosts.contains(*target)
                    && !self.enriching_hosts.contains(*target)
                    && self
                        .reconnect
                        .get(*target)
                        .is_some_and(|state| state.is_due(now))
            })
            .cloned()
            .collect::<Vec<_>>();
        due.sort_by_key(|target| (!self.is_important_target(target), target.clone()));

        for target in due.into_iter().take(capacity) {
            self.start_host_refresh(target);
        }
        self.update_refresh_status();
    }

    fn start_host_refresh(&mut self, target: String) {
        let Some(state) = self.reconnect.get_mut(&target) else {
            return;
        };
        state.begin_attempt();
        self.pending_hosts.insert(target.clone());
        let generation = self.refresh_generation;
        let sender = self.refresh_tx.clone();
        let limit = self.limit;
        let observed_host = agents::observed_host(&target);
        let cached = self
            .active
            .iter()
            .chain(&self.settled)
            .filter(|session| session.host == observed_host)
            .map(|session| {
                (
                    session.native_session_id.clone(),
                    (
                        session.last_interaction_unix_seconds,
                        session.updated_unix_seconds,
                        !session.last_message.is_empty(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        thread::spawn(move || match agents::discover(&target, Some(limit)) {
            Ok(sessions) => {
                if sender
                    .send(RefreshEvent::Basic {
                        generation,
                        target: target.clone(),
                        sessions: sessions.clone(),
                    })
                    .is_err()
                {
                    return;
                }

                let mut detailed = sessions
                    .into_iter()
                    .filter(|session| session_needs_detail(session, &cached))
                    .collect::<Vec<_>>();
                if detailed.is_empty() || agents::enrich_details(&target, &mut detailed).is_ok() {
                    let _ = sender.send(RefreshEvent::Detailed {
                        generation,
                        target,
                        sessions: detailed,
                    });
                } else {
                    let _ = sender.send(RefreshEvent::DetailFailed { generation, target });
                }
            }
            Err(error) => {
                let _ = sender.send(RefreshEvent::Failed {
                    generation,
                    target,
                    error: error.to_string(),
                });
            }
        });
    }

    pub(super) fn maybe_start_activity_poll(&mut self) {
        let now = Instant::now();
        let targets = self
            .discovery_targets
            .iter()
            .filter(|target| {
                if self.activity_pending.contains(*target)
                    || self.pending_hosts.contains(*target)
                    || self.host_errors.contains_key(*target)
                {
                    return false;
                }
                let interval = if target.as_str() == "local" {
                    LOCAL_ACTIVITY_POLL_INTERVAL
                } else {
                    REMOTE_ACTIVITY_POLL_INTERVAL
                };
                if self
                    .last_activity_poll
                    .get(*target)
                    .is_some_and(|started| started.elapsed() < interval)
                {
                    return false;
                }
                let observed_host = agents::observed_host(target);
                self.active.iter().chain(&self.settled).any(|session| {
                    session.host == observed_host
                        && self.fresh_ids.contains(&session.id)
                        && session.runtime != RuntimeOwner::Resumable
                })
            })
            .cloned()
            .collect::<Vec<_>>();

        for target in targets {
            self.last_activity_poll.insert(target.clone(), now);
            self.activity_pending.insert(target.clone());
            let sender = self.refresh_tx.clone();
            let generation = self.refresh_generation;
            thread::spawn(move || {
                let observations = agents::observe_live(&target).map_err(|error| error.to_string());
                let _ = sender.send(RefreshEvent::Activity {
                    generation,
                    target,
                    observations,
                });
            });
        }
    }

    pub(super) fn drain_refresh_events(&mut self) -> Result<(), PickerError> {
        while let Ok(event) = self.refresh_rx.try_recv() {
            self.apply_refresh_event(event)?;
        }
        Ok(())
    }

    pub(super) fn apply_refresh_event(&mut self, event: RefreshEvent) -> Result<(), PickerError> {
        let selection = self.selected_selection();
        match event {
            RefreshEvent::Basic {
                generation,
                target,
                mut sessions,
            } if generation == self.refresh_generation => {
                self.pending_hosts.remove(&target);
                self.initial_targets_remaining.remove(&target);
                self.host_errors.remove(&target);
                self.reachable_hosts.insert(target.clone());
                self.enriching_hosts.insert(target.clone());
                self.last_activity_poll.remove(&target);
                let important = self.is_important_target(&target);
                let recovered = self
                    .reconnect
                    .get_mut(&target)
                    .is_some_and(|state| state.succeeded(Instant::now(), important));
                self.mark_host_cached(&target);
                self.preserve_cached_messages(&mut sessions);
                self.record_fresh_sessions(&sessions)?;
                if recovered {
                    let observed_host = agents::observed_host(&target);
                    self.notice = Some((
                        format!(
                            "{} is back online · restored {} sessions",
                            observed_host,
                            sessions.len()
                        ),
                        Instant::now(),
                    ));
                    notify::host_recovered(&observed_host, sessions.len());
                }
            }
            RefreshEvent::Detailed {
                generation,
                target,
                mut sessions,
            } if generation == self.refresh_generation => {
                self.enriching_hosts.remove(&target);
                self.preserve_live_state(&mut sessions);
                self.record_fresh_sessions(&sessions)?;
            }
            RefreshEvent::Failed {
                generation,
                target,
                error,
            } if generation == self.refresh_generation => {
                self.pending_hosts.remove(&target);
                self.initial_targets_remaining.remove(&target);
                self.enriching_hosts.remove(&target);
                self.reachable_hosts.remove(&target);
                self.activity_pending.remove(&target);
                let important = self.is_important_target(&target);
                let class = classify_failure(&error);
                if let Some(state) = self.reconnect.get_mut(&target) {
                    state.failed(
                        Instant::now(),
                        &target,
                        important,
                        self.reconnect_jitter_seed,
                        class,
                    );
                }
                self.mark_host_cached(&target);
                self.host_errors.insert(target, error);
            }
            RefreshEvent::DetailFailed { generation, target }
                if generation == self.refresh_generation =>
            {
                self.enriching_hosts.remove(&target);
            }
            RefreshEvent::Activity {
                generation,
                target,
                observations,
            } if generation == self.refresh_generation => {
                self.activity_pending.remove(&target);
                if let Ok(observations) = observations {
                    self.apply_live_observations(&target, &observations)?;
                }
                return Ok(());
            }
            RefreshEvent::Preview {
                request_id,
                session_id,
                result,
            } => {
                if let Some(preview) = self.preview.as_mut()
                    && preview.request_id == request_id
                    && preview.session_id == session_id
                {
                    let fallback = self
                        .active
                        .iter()
                        .chain(&self.settled)
                        .find(|session| session.id == session_id)
                        .map(cached_preview)
                        .unwrap_or_else(|| "No cached response is available.".to_owned());
                    preview.content = match result {
                        Ok(body) if !body.trim().is_empty() => PreviewContent::Ready {
                            source: "live tmux pane",
                            body,
                        },
                        Ok(_) => PreviewContent::Ready {
                            source: "cached response",
                            body: fallback,
                        },
                        Err(error) => PreviewContent::Failed { error, fallback },
                    };
                    preview.scroll_from_bottom = 0;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }

        self.reload_snapshots()?;
        self.rebuild_host_choices();
        self.rebuild_rows_selecting(selection);
        self.update_refresh_status();
        Ok(())
    }

    pub(super) fn apply_live_observations(
        &mut self,
        target: &str,
        observations: &BTreeMap<String, LiveObservation>,
    ) -> Result<(), PickerError> {
        let selection = self.selected_selection();
        let observed_host = agents::observed_host(target);
        let mut updates = Vec::new();

        for session in self.active.iter_mut().chain(self.settled.iter_mut()) {
            if session.host != observed_host || !self.fresh_ids.contains(&session.id) {
                continue;
            }
            let before = (session.activity, session.runtime, session.tmux.clone());
            if let Some(observation) = observations.get(&session.id) {
                session.runtime = observation.runtime;
                session.tmux.clone_from(&observation.tmux);
                session.activity = merge_observed_activity(session.activity, observation.activity);
            } else if session.runtime != RuntimeOwner::Resumable {
                session.runtime = RuntimeOwner::Resumable;
                session.tmux = None;
                if session.activity == Activity::Working {
                    session.activity = Activity::Completed;
                }
            }
            if before != (session.activity, session.runtime, session.tmux.clone()) {
                updates.push(session.clone());
            }
        }

        if !updates.is_empty() {
            self.record_sessions(&updates)?;
            self.rebuild_rows_selecting(selection);
        }
        Ok(())
    }

    fn record_fresh_sessions(&mut self, sessions: &[Session]) -> Result<(), PickerError> {
        self.fresh_ids
            .extend(sessions.iter().map(|session| session.id.clone()));
        self.record_sessions(sessions)?;
        Ok(())
    }

    fn record_sessions(&mut self, sessions: &[Session]) -> Result<(), PickerError> {
        let transitions = self.store.record(sessions)?;
        self.handle_transitions(&transitions);
        Ok(())
    }

    pub(super) fn handle_transitions(&mut self, transitions: &[SessionTransition]) {
        if transitions.is_empty() {
            return;
        }
        for transition in transitions {
            self.unread_ids.insert(transition.session_id.clone());
            notify::session_transition(transition);
            if let Some(sender) = &self.transition_tx {
                let _ = sender.send(transition.clone());
            }
        }
        self.notice = if let [transition] = transitions {
            Some((
                format!(
                    "{} · {} on {}",
                    transition.kind.as_str(),
                    transition.title,
                    transition.host
                ),
                Instant::now(),
            ))
        } else {
            Some((
                format!("{} sessions need attention", transitions.len()),
                Instant::now(),
            ))
        };
    }

    fn preserve_cached_messages(&self, sessions: &mut [Session]) {
        for session in sessions {
            if !session.last_message.is_empty() {
                continue;
            }
            if let Some(cached) = self
                .active
                .iter()
                .chain(&self.settled)
                .find(|cached| cached.id == session.id)
            {
                session.last_message.clone_from(&cached.last_message);
            }
        }
    }

    fn preserve_live_state(&self, sessions: &mut [Session]) {
        for session in sessions {
            let Some(current) = self
                .active
                .iter()
                .chain(&self.settled)
                .find(|current| current.id == session.id)
            else {
                continue;
            };
            if current.runtime != RuntimeOwner::Resumable {
                session.runtime = current.runtime;
                session.tmux.clone_from(&current.tmux);
                session.activity = current.activity;
            }
        }
    }

    fn mark_host_cached(&mut self, target: &str) {
        let observed_host = agents::observed_host(target);
        for session in self.active.iter_mut().chain(self.settled.iter_mut()) {
            if session.host == observed_host {
                self.fresh_ids.remove(&session.id);
                session.runtime = RuntimeOwner::Resumable;
                session.tmux = None;
            }
        }
    }

    fn update_refresh_status(&mut self) {
        self.status = Some(if !self.pending_hosts.is_empty() {
            format!(
                "Checking {} hosts · cached sessions are ready",
                self.pending_hosts.len()
            )
        } else if !self.enriching_hosts.is_empty() {
            format!(
                "{} hosts reachable · loading message details from {}",
                self.reachable_hosts.len(),
                self.enriching_hosts.len()
            )
        } else if self.host_errors.is_empty() {
            format!(
                "Loaded {} active and {} settled sessions from {} hosts.",
                self.active.len(),
                self.settled.len(),
                self.reachable_hosts.len()
            )
        } else {
            format!(
                "Loaded {} active · {} reachable · {} unavailable · retries are automatic",
                self.active.len(),
                self.reachable_hosts.len(),
                self.host_errors.len()
            )
        });
    }

    pub(super) fn expire_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, shown_at)| shown_at.elapsed() >= NOTICE_DURATION)
        {
            self.notice = None;
        }
    }

    pub(super) fn initial_refresh_complete(&self) -> bool {
        self.initial_targets_remaining.is_empty()
            && self.pending_hosts.is_empty()
            && self.enriching_hosts.is_empty()
            && self.activity_pending.is_empty()
    }

    pub(super) fn target_for_observed_host(&self, host: &str) -> Option<&String> {
        self.discovery_targets
            .iter()
            .find(|target| agents::observed_host(target) == host)
    }

    fn is_important_target(&self, target: &str) -> bool {
        if target == "local" {
            return true;
        }
        let observed_host = agents::observed_host(target);
        if self.host_filter == observed_host
            || self
                .selected_session()
                .is_some_and(|session| session.host == observed_host)
        {
            return true;
        }
        self.active.iter().any(|session| {
            session.host == observed_host
                && matches!(
                    session.activity,
                    Activity::Working
                        | Activity::WaitingApproval
                        | Activity::WaitingInput
                        | Activity::Failed
                )
        })
    }

    pub(super) fn reload_snapshots(&mut self) -> Result<(), PickerError> {
        self.store.auto_settle_stale(DEFAULT_STALE_AFTER_SECONDS)?;
        self.active = self.store.load(false)?;
        self.settled = self.store.load(true)?;
        self.unread_ids = self.store.unread_session_ids()?.into_iter().collect();
        for session in self.active.iter_mut().chain(self.settled.iter_mut()) {
            if !self.fresh_ids.contains(&session.id) {
                session.runtime = RuntimeOwner::Resumable;
                session.tmux = None;
            }
        }
        Ok(())
    }

    pub(super) fn acknowledge_session(
        &mut self,
        session_id: &str,
        show_status: bool,
    ) -> Result<(), PickerError> {
        let title = self
            .active
            .iter()
            .chain(&self.settled)
            .find(|session| session.id == session_id)
            .map_or_else(|| session_id.to_owned(), |session| session.title.clone());
        let changed = self.store.acknowledge(session_id)?;
        self.unread_ids.remove(session_id);
        self.rebuild_rows_selecting(Some(DashboardSelection::Session(session_id.to_owned())));
        if show_status {
            let message = if changed {
                format!("Marked {title} read.")
            } else {
                format!("{title} was already read.")
            };
            if let Some(preview) = self.preview.as_mut() {
                preview.notice = Some(message);
            } else {
                self.notice = Some((message, Instant::now()));
            }
        }
        Ok(())
    }

    pub(super) fn set_archived(
        &mut self,
        session_id: &str,
        archived: bool,
    ) -> Result<(), PickerError> {
        let selection = self.selected_selection();
        let title = self
            .active
            .iter()
            .chain(&self.settled)
            .find(|session| session.id == session_id)
            .map_or_else(|| session_id.to_owned(), |session| session.title.clone());
        self.store.set_archived(session_id, archived)?;
        self.reload_snapshots()?;
        self.rebuild_rows_selecting(selection);
        self.notice = Some((
            if archived {
                format!("Settled {title}.")
            } else {
                format!("Restored {title}.")
            },
            Instant::now(),
        ));
        Ok(())
    }
}

pub(super) fn normalize_host_filter(host: &str) -> String {
    if host == ALL_HOSTS {
        ALL_HOSTS.to_owned()
    } else {
        agents::observed_host(host)
    }
}

pub(super) fn reconnect_jitter_seed() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let folded_time = u64::try_from(time ^ (time >> 64)).unwrap_or_default();
    folded_time ^ u64::from(std::process::id())
}

pub(super) fn discovery_targets() -> Result<Vec<String>, HostDiscoveryError> {
    Ok(deduplicate_discovery_targets(hosts::discover(None)?))
}

pub(super) fn restrict_discovery_targets(targets: Vec<String>, host: &str) -> Vec<String> {
    if host == ALL_HOSTS {
        return targets;
    }
    if host == "local" {
        return vec!["local".to_owned()];
    }
    if targets.iter().any(|target| target == host) {
        return vec![host.to_owned()];
    }

    let observed_host = normalize_host_filter(host);
    let matching = targets
        .into_iter()
        .filter(|target| target == host || agents::observed_host(target) == observed_host)
        .collect::<Vec<_>>();
    if matching.is_empty() {
        vec![host.to_owned()]
    } else {
        matching
    }
}

pub(super) fn deduplicate_discovery_targets(hosts: Vec<SshHost>) -> Vec<String> {
    let mut endpoints: BTreeMap<(String, Option<String>, u16), String> = BTreeMap::new();
    for host in hosts
        .into_iter()
        .filter(|host| host.resolution_error.is_none())
    {
        let key = (host.hostname.to_lowercase(), host.user.clone(), host.port);
        endpoints
            .entry(key)
            .and_modify(|current| {
                if (host.alias.len(), &host.alias) < (current.len(), current) {
                    current.clone_from(&host.alias);
                }
            })
            .or_insert(host.alias);
    }
    let mut targets = endpoints.into_values().collect::<Vec<_>>();
    targets.sort();
    targets.insert(0, "local".to_owned());
    targets
}

pub(super) fn session_needs_detail(
    session: &Session,
    cached: &BTreeMap<String, (u64, u64, bool)>,
) -> bool {
    cached
        .get(&session.native_session_id)
        .is_none_or(|(last_interaction, updated, has_message)| {
            !has_message
                || *last_interaction != session.last_interaction_unix_seconds
                || *updated != session.updated_unix_seconds
        })
}
