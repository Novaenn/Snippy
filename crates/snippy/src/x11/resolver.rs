use super::UnknownReason;
use snippy_core::source::SourceIdentity;
use x11rb::{
    protocol::{
        res::{ClientIdMask, ClientIdSpec, ClientIdValue, ConnectionExt as _},
        xproto::{AtomEnum, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

const MAX_CLASS_BYTES: u32 = 1024;
const MAX_WINDOWS: usize = 128;
const MAX_CLIENTS: usize = 1024;

pub(super) fn resolve(
    connection: &RustConnection,
    root: u32,
    client_list: u32,
    owner: u32,
    has_res: bool,
    has_res_pids: bool,
) -> (SourceIdentity, Option<UnknownReason>) {
    if owner == 0 {
        return unknown(UnknownReason::Empty);
    }
    if let Some(class) = window_class(connection, owner) {
        return identity(class);
    }
    if !has_res {
        return unknown(UnknownReason::MissingMetadata);
    }
    // Server resource ranges, not a spoofable PID property or current focus.
    let clients = connection
        .res_query_clients()
        .ok()
        .and_then(|c| c.reply().ok());
    let Some(clients) = clients.map(|reply| reply.clients) else {
        return unknown(UnknownReason::MissingMetadata);
    };
    if clients.len() > MAX_CLIENTS {
        return unknown(UnknownReason::ResolutionBudget);
    }
    let client_for = |window: u32| {
        clients
            .iter()
            .find(|client| window & !client.resource_mask == client.resource_base)
    };
    let Some(client) = client_for(owner) else {
        return unknown(UnknownReason::MissingMetadata);
    };
    let same_client = |window: u32| window & !client.resource_mask == client.resource_base;
    let mut candidates = Vec::new();
    let mut window = owner;
    for _ in 0..8 {
        let Some(tree) = connection
            .query_tree(window)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            break;
        };
        if tree.parent == root || tree.parent == 0 || tree.parent == window {
            break;
        }
        window = tree.parent;
        if same_client(window) {
            candidates.push(window);
        }
    }
    let property = connection
        .get_property(
            false,
            root,
            client_list,
            AtomEnum::WINDOW,
            0,
            MAX_WINDOWS as u32,
        )
        .ok()
        .and_then(|c| c.reply().ok());
    let windows = if let Some(property) =
        property.filter(|p| p.type_ == u32::from(AtomEnum::WINDOW) && p.format == 32)
    {
        if property.bytes_after != 0 {
            return unknown(UnknownReason::ResolutionBudget);
        }
        property
            .value32()
            .map(|values| values.collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        let Some(tree) = connection
            .query_tree(root)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return unknown(UnknownReason::MissingMetadata);
        };
        if tree.children.len() > MAX_WINDOWS {
            return unknown(UnknownReason::ResolutionBudget);
        }
        tree.children
    };
    candidates.extend(
        windows
            .iter()
            .copied()
            .filter(|window| same_client(*window)),
    );
    candidates.sort_unstable();
    candidates.dedup();
    let mut classes = Vec::new();
    if let Err(reason) = collect_classes(connection, candidates, &mut classes) {
        return unknown(reason);
    }
    if let Some(class) = classes.pop() {
        return identity(class);
    }
    if !has_res_pids {
        return unknown(UnknownReason::MissingMetadata);
    }

    // Clipboard libraries can open a separate X11 connection in the same
    // process (e.g. Alacritty). Match server-reported local PIDs, never the
    // forgeable _NET_WM_PID property, focus, or a process command line.
    let mut bases = windows
        .iter()
        .filter_map(|window| client_for(*window))
        .map(|client| client.resource_base)
        .collect::<Vec<_>>();
    bases.push(client.resource_base);
    bases.sort_unstable();
    bases.dedup();
    let specs = bases
        .iter()
        .map(|base| ClientIdSpec {
            client: *base,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        })
        .collect::<Vec<_>>();
    let ids = connection
        .res_query_client_ids(&specs)
        .ok()
        .and_then(|c| c.reply().ok());
    let Some(ids) = ids else {
        return unknown(UnknownReason::MissingMetadata);
    };
    let same_process = matching_clients(client.resource_base, &bases, &ids.ids);
    let candidates = windows.into_iter().filter(|window| {
        client_for(*window).is_some_and(|client| same_process.contains(&client.resource_base))
    });
    if let Err(reason) = collect_classes(connection, candidates, &mut classes) {
        return unknown(reason);
    }
    match classes.pop() {
        Some(class) => identity(class),
        None => unknown(UnknownReason::MissingMetadata),
    }
}

fn collect_classes(
    connection: &RustConnection,
    candidates: impl IntoIterator<Item = u32>,
    classes: &mut Vec<String>,
) -> Result<(), UnknownReason> {
    for window in candidates {
        if let Some(class) = window_class(connection, window) {
            if is_proxy(&class) {
                return Err(UnknownReason::ClipboardProxy);
            }
            if !classes.contains(&class) {
                classes.push(class);
            }
            if classes.len() > 1 {
                return Err(UnknownReason::AmbiguousClient);
            }
        }
    }
    Ok(())
}

fn matching_clients(owner_base: u32, requested: &[u32], ids: &[ClientIdValue]) -> Vec<u32> {
    let pid_for = |base| {
        let mut values = ids
            .iter()
            .filter(|id| id.spec.client == base && id.spec.mask == ClientIdMask::LOCAL_CLIENT_PID);
        let value = values.next()?;
        if values.next().is_some() || value.value.len() != 1 || value.value[0] == 0 {
            return None;
        }
        Some(value.value[0])
    };
    let Some(owner_pid) = pid_for(owner_base) else {
        return Vec::new();
    };
    requested
        .iter()
        .copied()
        .filter(|base| pid_for(*base) == Some(owner_pid))
        .collect()
}

fn window_class(connection: &RustConnection, window: u32) -> Option<String> {
    let reply = connection
        .get_property(
            false,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            0,
            MAX_CLASS_BYTES / 4,
        )
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ != u32::from(AtomEnum::STRING) || reply.format != 8 || reply.bytes_after != 0 {
        return None;
    }
    parse_class(&reply.value)
}

fn parse_class(bytes: &[u8]) -> Option<String> {
    let mut fields = bytes.split(|byte| *byte == 0);
    let _instance = fields.next()?;
    let class = std::str::from_utf8(fields.next()?).ok()?;
    if class.is_empty() || class.len() > 256 || class.chars().any(char::is_control) {
        return None;
    }
    Some(class.to_owned())
}

fn is_proxy(class: &str) -> bool {
    matches!(
        class.to_ascii_lowercase().as_str(),
        "copyq"
            | "klipper"
            | "org.kde.klipper"
            | "xfce4-clipman"
            | "clipman"
            | "parcellite"
            | "clipit"
            | "diodon"
            | "greenclip"
            | "gpaste"
    )
}

fn identity(class: String) -> (SourceIdentity, Option<UnknownReason>) {
    if is_proxy(&class) {
        return unknown(UnknownReason::ClipboardProxy);
    }
    (
        SourceIdentity::Observed {
            app_id: format!("x11:wm-class:{class}"),
            display_name: class,
        },
        None,
    )
}
fn unknown(reason: UnknownReason) -> (SourceIdentity, Option<UnknownReason>) {
    (SourceIdentity::Unknown, Some(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn class_is_the_second_icccm_field_not_the_instance_or_title() {
        assert_eq!(
            parse_class(b"profile-1\0Firefox\0").as_deref(),
            Some("Firefox")
        );
        assert_eq!(parse_class(b"missing-class"), None);
        assert_eq!(parse_class(b"name\0bad\nclass\0"), None);
        assert_eq!(parse_class(b"name\0\xff\0"), None);
    }
    #[test]
    fn proxies_never_claim_original_source_identity() {
        let (source, reason) = identity("CopyQ".into());
        assert!(source == SourceIdentity::Unknown);
        assert_eq!(reason, Some(UnknownReason::ClipboardProxy));
    }

    #[test]
    fn process_matching_requires_one_nonzero_server_pid_per_requested_client() {
        let id = |client, pid| ClientIdValue {
            spec: ClientIdSpec {
                client,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            },
            value: vec![pid],
        };
        let ids = vec![id(100, 42), id(200, 42), id(300, 99), id(400, 0)];
        assert_eq!(
            matching_clients(100, &[100, 200, 300, 400], &ids),
            vec![100, 200]
        );
        assert!(matching_clients(400, &[100, 400], &ids).is_empty());
        assert!(matching_clients(500, &[100, 200], &ids).is_empty());
        let duplicate = vec![id(100, 42), id(100, 99), id(200, 42)];
        assert!(matching_clients(100, &[100, 200], &duplicate).is_empty());
        let malformed = vec![ClientIdValue {
            spec: ClientIdSpec {
                client: 100,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            },
            value: vec![42, 99],
        }];
        assert!(matching_clients(100, &[100], &malformed).is_empty());
        let wrong_type = vec![ClientIdValue {
            spec: ClientIdSpec {
                client: 100,
                mask: ClientIdMask::CLIENT_XID,
            },
            value: vec![42],
        }];
        assert!(matching_clients(100, &[100], &wrong_type).is_empty());
    }
}
