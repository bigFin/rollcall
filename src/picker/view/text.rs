use std::{
    env,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::domain::{Activity, RuntimeOwner, Session};

pub(in crate::picker) fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Working => "working",
        Activity::WaitingApproval => "approval",
        Activity::WaitingInput => "input",
        Activity::Completed => "completed",
        Activity::Failed => "failed",
        Activity::Unknown => "unknown",
    }
}

pub(in crate::picker) fn display_directory(cwd: &str) -> String {
    let compact = compact_home(cwd);
    let path = Path::new(&compact);
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&compact)
        .to_owned()
}

pub(in crate::picker) fn compact_home(path: &str) -> String {
    let Some(home) = env::var_os("HOME") else {
        return path.to_owned();
    };
    let home = home.to_string_lossy();
    if path == home {
        "~".to_owned()
    } else if let Some(relative) = path.strip_prefix(home.as_ref()) {
        if relative.starts_with('/') {
            format!("~{relative}")
        } else {
            path.to_owned()
        }
    } else {
        path.to_owned()
    }
}

pub(in crate::picker) fn truncate_with_ellipsis(value: &str, maximum_characters: usize) -> String {
    if maximum_characters == 0 {
        return String::new();
    }
    let mut characters = value.chars();
    let prefix = characters
        .by_ref()
        .take(maximum_characters)
        .collect::<String>();
    if characters.next().is_some() {
        if maximum_characters == 1 {
            "…".to_owned()
        } else {
            format!(
                "{}…",
                prefix
                    .chars()
                    .take(maximum_characters.saturating_sub(1))
                    .collect::<String>()
            )
        }
    } else {
        prefix
    }
}

pub(in crate::picker) fn cached_preview(session: &Session) -> String {
    if session.last_message.trim().is_empty() {
        format!(
            "{}\n\nNo cached agent response is available.\n\n{} / {}",
            session.title,
            compact_home(&session.cwd),
            session.host
        )
    } else {
        format!(
            "{}\n\n{}\n\n{} / {}",
            session.title,
            session.last_message,
            compact_home(&session.cwd),
            session.host
        )
    }
}

pub(in crate::picker) const fn cached_preview_source(session: &Session) -> &'static str {
    match session.runtime {
        RuntimeOwner::ExternalFrontend => "external frontend · cached response",
        RuntimeOwner::SharedBackend => "Rollcall backend · cached response",
        RuntimeOwner::TmuxFrontend | RuntimeOwner::Resumable => "cached response",
    }
}

pub(in crate::picker) fn wrap_preview_body(body: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let characters = line.chars().collect::<Vec<_>>();
        wrapped.extend(
            characters
                .chunks(width)
                .map(|chunk| chunk.iter().collect::<String>()),
        );
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}
pub(in crate::picker) fn preview_window(
    lines: &[String],
    height: usize,
    scroll_from_bottom: usize,
) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let maximum_scroll = lines.len().saturating_sub(height);
    let scroll = scroll_from_bottom.min(maximum_scroll);
    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    lines[start..end].join("\n")
}

pub(in crate::picker) fn format_age(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(timestamp, |duration| duration.as_secs());
    let seconds = now.saturating_sub(timestamp);

    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}
