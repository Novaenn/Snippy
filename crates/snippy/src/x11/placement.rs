//! Optional placement: XRes verifies the GUI process; RandR selects the pointer monitor.
use x11rb::{
    protocol::{
        randr::ConnectionExt as _,
        res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
        xproto::{AtomEnum, ConfigureWindowAux, ConnectionExt as _, MapState, WindowClass},
    },
    rust_connection::RustConnection,
};
pub(super) fn position(
    c: &RustConnection,
    root: u32,
    client_list: u32,
    pid: u32,
    has_res_pids: bool,
) {
    let _ = position_inner(c, root, client_list, pid, has_res_pids);
}
fn position_inner(
    c: &RustConnection,
    root: u32,
    client_list: u32,
    pid: u32,
    has_res_pids: bool,
) -> Option<()> {
    if !has_res_pids {
        return None;
    }
    let clients = c.res_query_clients().ok()?.reply().ok()?.clients;
    if clients.len() > 1024 {
        return None;
    }
    let property = c
        .get_property(false, root, client_list, AtomEnum::WINDOW, 0, 128)
        .ok()?
        .reply()
        .ok()?;
    let windows = if property.type_ == u32::from(AtomEnum::WINDOW) && property.bytes_after == 0 {
        property.value32()?.collect::<Vec<_>>()
    } else {
        let windows = c.query_tree(root).ok()?.reply().ok()?.children;
        if windows.len() > 128 {
            return None;
        }
        windows
    };
    let mut bases = windows
        .iter()
        .filter_map(|w| {
            clients
                .iter()
                .find(|client| *w & !client.resource_mask == client.resource_base)
        })
        .map(|client| client.resource_base)
        .collect::<Vec<_>>();
    bases.sort_unstable();
    bases.dedup();
    let specs = bases
        .iter()
        .map(|base| ClientIdSpec {
            client: *base,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        })
        .collect::<Vec<_>>();
    let ids = c.res_query_client_ids(&specs).ok()?.reply().ok()?.ids;
    let matching = bases
        .into_iter()
        .filter(|base| {
            let mut ids = ids.iter().filter(|id| {
                id.spec.client == *base && id.spec.mask == ClientIdMask::LOCAL_CLIENT_PID
            });
            let id = ids.next();
            ids.next().is_none() && id.is_some_and(|id| id.value == [pid])
        })
        .collect::<Vec<_>>();
    let screen = c.get_geometry(root).ok()?.reply().ok()?;
    let pointer = c.query_pointer(root).ok()?.reply().ok()?;
    let mut area = (0i32, 0i32, screen.width as i32, screen.height as i32);
    if c.randr_query_version(1, 5)
        .ok()
        .and_then(|q| q.reply().ok())
        .is_some_and(|r| (r.major_version, r.minor_version) >= (1, 5))
        && let Some(monitors) = c
            .randr_get_monitors(root, true)
            .ok()
            .and_then(|q| q.reply().ok())
        && let Some(m) = monitors.monitors.iter().find(|m| {
            pointer.root_x as i32 >= m.x as i32
                && pointer.root_y as i32 >= m.y as i32
                && (pointer.root_x as i32) < m.x as i32 + m.width as i32
                && (pointer.root_y as i32) < m.y as i32 + m.height as i32
        })
    {
        area = (m.x as i32, m.y as i32, m.width as i32, m.height as i32);
    }
    for window in windows {
        if !clients
            .iter()
            .find(|client| window & !client.resource_mask == client.resource_base)
            .is_some_and(|client| matching.contains(&client.resource_base))
        {
            continue;
        }
        // A listed window may vanish before it is queried; skip only that one.
        let Some(attributes) = c
            .get_window_attributes(window)
            .ok()
            .and_then(|r| r.reply().ok())
        else {
            continue;
        };
        if attributes.class != WindowClass::INPUT_OUTPUT
            || attributes.map_state != MapState::VIEWABLE
            || attributes.override_redirect
        {
            continue;
        }
        let Some(size) = c.get_geometry(window).ok().and_then(|r| r.reply().ok()) else {
            continue;
        };
        let x = area.0 + (area.2 - size.width as i32).max(0) / 2;
        let y = area.1 + (area.3 - size.height as i32).max(0) / 2;
        let _ = c
            .configure_window(window, &ConfigureWindowAux::new().x(x).y(y))
            .map(|cookie| cookie.check());
    }
    use x11rb::connection::Connection;
    c.flush().ok()?;
    Some(())
}
