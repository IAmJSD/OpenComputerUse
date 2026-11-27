//! The VNC (RFB 003.008, RFC 6143) server: lets any VNC client (macOS
//! Screen Sharing, TigerVNC, RealVNC) watch and drive a background session
//! through its [`Viewer`], using the view token as the VNC password.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context as _, Result};

use ocu_core::image::Order;
use ocu_core::{Frame, MouseButton, Service, SessionInfo, ViewInput, Viewer};

use super::protocol::{changed, KeyAction, Keys, Region};

/// Default framebuffer size when a VNC client connects before any session
/// or window is open.
const DEFAULT_WIDTH: u16 = 1280;
const DEFAULT_HEIGHT: u16 = 800;
const MAX_CUT_TEXT: usize = 64 << 10;
const ENCODING_RAW: i32 = 0;
const ENCODING_DESKTOP_SIZE: i32 = -223;

pub fn start(service: Arc<Service>, addr: SocketAddr, token: String) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("can't listen on {addr}"))?;
    service.share_sessions();
    std::thread::Builder::new()
        .name("vnc".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let (service, token) = (service.clone(), token.clone());
                let _ = std::thread::Builder::new()
                    .name("vnc-conn".into())
                    .spawn(move || {
                        if let Err(e) = connection(&service, &token, stream) {
                            log::debug!("vnc connection: {e:#}");
                        }
                    });
            }
        })?;
    log::warn!("VNC server listening on {addr}");
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    Rfb33,
    Rfb37,
    Rfb38,
}

/// RFB `PIXEL_FORMAT` (16 bytes, RFC 6143 §7.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl Default for PixelFormat {
    fn default() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_color: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }
}

impl PixelFormat {
    pub fn to_bytes(self) -> [u8; 16] {
        let [rm0, rm1] = self.red_max.to_be_bytes();
        let [gm0, gm1] = self.green_max.to_be_bytes();
        let [bm0, bm1] = self.blue_max.to_be_bytes();
        [
            self.bits_per_pixel,
            self.depth,
            u8::from(self.big_endian),
            u8::from(self.true_color),
            rm0,
            rm1,
            gm0,
            gm1,
            bm0,
            bm1,
            self.red_shift,
            self.green_shift,
            self.blue_shift,
            0,
            0,
            0,
        ]
    }

    pub fn from_bytes(b: &[u8; 16]) -> Result<Self> {
        let bpp = b[0];
        ensure!(
            matches!(bpp, 8 | 16 | 32),
            "unsupported VNC bits_per_pixel {bpp}"
        );
        ensure!(
            b[3] != 0,
            "VNC colour-map (non-true-colour) pixel format is not supported"
        );
        let red_max = u16::from_be_bytes([b[4], b[5]]);
        let green_max = u16::from_be_bytes([b[6], b[7]]);
        let blue_max = u16::from_be_bytes([b[8], b[9]]);
        ensure!(
            red_max > 0 && green_max > 0 && blue_max > 0,
            "invalid VNC colour max 0"
        );
        Ok(Self {
            bits_per_pixel: bpp,
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: true,
            red_max,
            green_max,
            blue_max,
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        })
    }
}

/// Converts `region` of `frame` into the client's negotiated [`PixelFormat`].
pub fn encode_raw(frame: &Frame, region: Region, pf: PixelFormat) -> Vec<u8> {
    let (w, h) = (region.width as usize, region.height as usize);
    let bpp_bytes = (pf.bits_per_pixel / 8) as usize;
    let mut out = Vec::with_capacity(w * h * bpp_bytes);

    let fast_bgra = pf.bits_per_pixel == 32
        && !pf.big_endian
        && pf.true_color
        && pf.red_max == 255
        && pf.green_max == 255
        && pf.blue_max == 255
        && pf.red_shift == 16
        && pf.green_shift == 8
        && pf.blue_shift == 0;

    for y in region.y as usize..region.y as usize + h {
        let row_start = y * frame.stride + region.x as usize * 4;
        let row = &frame.data[row_start..row_start + w * 4];
        if fast_bgra && frame.order == Order::Bgra {
            out.extend_from_slice(row);
            continue;
        }
        for px in row.as_chunks::<4>().0 {
            let (r, g, b) = match frame.order {
                Order::Bgra => (px[2], px[1], px[0]),
                Order::Rgba => (px[0], px[1], px[2]),
            };
            if fast_bgra {
                out.extend_from_slice(&[b, g, r, 0]);
                continue;
            }
            let rm = pf.red_max.max(1) as u32;
            let gm = pf.green_max.max(1) as u32;
            let bm = pf.blue_max.max(1) as u32;
            let (rs, gs, bs) = (pf.red_shift, pf.green_shift, pf.blue_shift);
            let rv = ((r as u32 * rm + 127) / 255) & rm;
            let gv = ((g as u32 * gm + 127) / 255) & gm;
            let bv = ((b as u32 * bm + 127) / 255) & bm;
            let val = (rv << (rs & 31)) | (gv << (gs & 31)) | (bv << (bs & 31));
            match (pf.bits_per_pixel, pf.big_endian) {
                (8, _) => out.push(val as u8),
                (16, true) => out.extend_from_slice(&(val as u16).to_be_bytes()),
                (16, false) => out.extend_from_slice(&(val as u16).to_le_bytes()),
                (_, true) => out.extend_from_slice(&val.to_be_bytes()),
                (_, false) => out.extend_from_slice(&val.to_le_bytes()),
            }
        }
    }
    out
}

/// Crops or pads `frame` into a canvas of size `(width, height)`, for VNC
/// clients that did not advertise the `DesktopSize` pseudo-encoding.
pub fn fit_frame(frame: &Frame, width: u16, height: u16) -> Frame {
    let (tw, th) = (width as u32, height as u32);
    if frame.width == tw && frame.height == th {
        return frame.clone();
    }
    let stride = tw as usize * 4;
    let mut data = vec![0u8; stride * th as usize];
    let (copy_w, copy_h) = (
        frame.width.min(tw) as usize * 4,
        frame.height.min(th) as usize,
    );
    for y in 0..copy_h {
        data[y * stride..y * stride + copy_w]
            .copy_from_slice(&frame.data[y * frame.stride..y * frame.stride + copy_w]);
    }
    Frame {
        window_id: frame.window_id,
        width: tw,
        height: th,
        stride,
        data,
        order: frame.order,
    }
}

/// Numeric sequence `n` from a session id `s{n}-{hash}`, so `"s10-..."` sorts
/// after `"s2-..."`.
pub fn session_seq(id: &str) -> u64 {
    id.strip_prefix('s')
        .and_then(|s| s.split_once('-'))
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or(0)
}

fn blank_frame(width: u16, height: u16) -> Frame {
    let (w, h) = (width as u32, height as u32);
    Frame {
        window_id: 0,
        width: w,
        height: h,
        stride: w as usize * 4,
        data: vec![0x14; w as usize * 4 * h as usize],
        order: Order::Bgra,
    }
}

fn latest_watchable(service: &Service) -> Option<(SessionInfo, Arc<dyn Viewer>)> {
    let mut sessions = service.sessions();
    sessions.sort_by_key(|s| session_seq(&s.id));
    sessions
        .into_iter()
        .rev()
        .find_map(|info| service.viewer(&info.id).ok().map(|v| (info, v)))
}

fn read_exact_timeout(stream: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let res = stream.read_exact(buf);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(10)));
    Ok(res?)
}

fn connection(service: &Service, token: &str, mut stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    stream.set_nodelay(true)?;

    stream.write_all(b"RFB 003.008\n")?;
    stream.flush()?;
    let mut ver_buf = [0u8; 12];
    stream.read_exact(&mut ver_buf)?;
    let proto = parse_version(&ver_buf)?;

    if proto == Protocol::Rfb33 {
        stream.write_all(&2u32.to_be_bytes())?;
        stream.flush()?;
    } else {
        stream.write_all(&[1u8, 2u8])?;
        stream.flush()?;
        let mut chosen = [0u8; 1];
        stream.read_exact(&mut chosen)?;
        ensure!(chosen[0] == 2, "unsupported security type {}", chosen[0]);
    }

    let mut challenge = [0u8; 16];
    getrandom::fill(&mut challenge).map_err(|e| anyhow!("no randomness: {e}"))?;
    stream.write_all(&challenge)?;
    stream.flush()?;

    let mut response = [0u8; 16];
    stream.read_exact(&mut response)?;
    let expected = vnc_encrypt_challenge(&challenge, token.as_bytes());
    if response
        .iter()
        .zip(&expected)
        .fold(0u8, |d, (x, y)| d | (x ^ y))
        != 0
    {
        std::thread::sleep(Duration::from_millis(200));
        stream.write_all(&1u32.to_be_bytes())?;
        if proto == Protocol::Rfb38 {
            let msg = b"wrong VNC password (use the first 8 characters of --view-token)";
            stream.write_all(&(msg.len() as u32).to_be_bytes())?;
            stream.write_all(msg)?;
        }
        let _ = stream.flush();
        bail!("VNC authentication failed");
    }
    stream.write_all(&0u32.to_be_bytes())?;
    stream.flush()?;
    stream.read_exact(&mut [0u8; 1])?;

    let mut active = latest_watchable(service);
    let mut seen: Option<u64> = None;
    let (mut fb_width, mut fb_height, name) = match &active {
        Some((info, viewer)) => {
            let title = format!("{} ({})", info.app, info.id);
            let (w, h) = if let Ok(f) = viewer.frame(None) {
                (
                    f.width.clamp(1, 4096) as u16,
                    f.height.clamp(1, 4096) as u16,
                )
            } else if let Ok(Some(w)) = viewer.windows().map(|ws| ws.into_iter().next()) {
                (
                    w.frame.width.round().clamp(1.0, 4096.0) as u16,
                    w.frame.height.round().clamp(1.0, 4096.0) as u16,
                )
            } else {
                (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            };
            (w, h, title)
        }
        None => (DEFAULT_WIDTH, DEFAULT_HEIGHT, "OpenComputerUse".to_string()),
    };
    let pf = PixelFormat::default();
    let mut server_init = Vec::with_capacity(24 + name.len());
    server_init.extend_from_slice(&fb_width.to_be_bytes());
    server_init.extend_from_slice(&fb_height.to_be_bytes());
    server_init.extend_from_slice(&pf.to_bytes());
    server_init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    server_init.extend_from_slice(name.as_bytes());
    stream.write_all(&server_init)?;
    stream.flush()?;

    // 5. Main non-blocking loop: input first, then framebuffer updates.
    stream.set_read_timeout(Some(Duration::from_millis(10)))?;
    let mut state = ClientState {
        pf,
        supports_desktop_size: false,
        pending_update: None,
        last: None,
        keys: Keys::default(),
        pointer: PointerState::default(),
    };
    let (mut sent, mut last_session_check) =
        (Instant::now() - Duration::from_secs(1), Instant::now());

    let result: Result<()> = 'main: loop {
        loop {
            let mut msg_type = [0u8; 1];
            match stream.read(&mut msg_type) {
                Ok(0) => break 'main Ok(()),
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => break 'main Err(e.into()),
            }
            if let Err(e) = handle_client_msg(
                msg_type[0],
                &mut stream,
                &mut state,
                active.as_ref().map(|(_, v)| &**v),
            ) {
                break 'main Err(e);
            }
        }

        let still_alive = active
            .as_ref()
            .is_some_and(|(info, v)| v.is_alive() && service.session_info(&info.id).is_some());
        if !still_alive || last_session_check.elapsed() >= Duration::from_millis(500) {
            last_session_check = Instant::now();
            let candidate = latest_watchable(service);
            let changed_session = match (&active, &candidate) {
                (Some((a, _)), Some((b, _))) => a.id != b.id || !still_alive,
                (None, None) => false,
                _ => true,
            };
            if changed_session {
                let released = state
                    .keys
                    .release_all()
                    .into_iter()
                    .chain(state.pointer.release_all());
                if let Some((_, old_viewer)) = active.take() {
                    for input in released {
                        let _ = old_viewer.input(None, &input);
                    }
                }
                (state.last, seen, active) = (None, None, candidate);
            }
        }

        let Some(force_full) = state.pending_update else {
            continue;
        };
        let interval = match &active {
            Some((_, v)) if v.generation().is_some() => Duration::from_millis(33),
            _ => Duration::from_millis(100),
        };
        if !force_full && sent.elapsed() < interval {
            continue;
        }
        let generation = active.as_ref().and_then(|(_, v)| v.generation());
        if !force_full && generation.is_some() && generation == seen && state.last.is_some() {
            continue;
        }
        let raw_frame = match &active {
            Some((_, v)) => match v.frame(None) {
                Ok(f) if f.width > 0 && f.height > 0 => f,
                _ if force_full => blank_frame(fb_width, fb_height),
                _ => continue,
            },
            None if force_full => blank_frame(fb_width, fb_height),
            None => continue,
        };
        (sent, seen) = (Instant::now(), generation);

        let (fw, fh) = (
            raw_frame.width.clamp(1, 4096) as u16,
            raw_frame.height.clamp(1, 4096) as u16,
        );
        let (mut frame, mut resize_rect) = (raw_frame, false);
        if (fw, fh) != (fb_width, fb_height) {
            if state.supports_desktop_size {
                (fb_width, fb_height, state.last, resize_rect) = (fw, fh, None, true);
            } else {
                frame = fit_frame(&frame, fb_width, fb_height);
            }
        }

        let region = if force_full || resize_rect {
            Some(Region {
                x: 0,
                y: 0,
                width: frame.width,
                height: frame.height,
            })
        } else {
            changed(state.last.as_ref(), &frame)
        };
        let Some(region) = region else {
            continue;
        };

        let pixels = encode_raw(&frame, region, state.pf);
        let n_rects: u16 = if resize_rect { 2 } else { 1 };
        let mut msg = Vec::with_capacity(4 + (n_rects as usize) * 12 + pixels.len());
        msg.extend_from_slice(&[0, 0]);
        msg.extend_from_slice(&n_rects.to_be_bytes());
        if resize_rect {
            for v in [0u16, 0u16, fb_width, fb_height] {
                msg.extend_from_slice(&v.to_be_bytes());
            }
            msg.extend_from_slice(&ENCODING_DESKTOP_SIZE.to_be_bytes());
        }
        for v in [
            region.x as u16,
            region.y as u16,
            region.width as u16,
            region.height as u16,
        ] {
            msg.extend_from_slice(&v.to_be_bytes());
        }
        msg.extend_from_slice(&ENCODING_RAW.to_be_bytes());
        msg.extend_from_slice(&pixels);
        if let Err(e) = stream.write_all(&msg).and_then(|_| stream.flush()) {
            break Err(e.into());
        }
        (state.last, state.pending_update) = (Some(frame), None);
    };

    if let Some((_, viewer)) = active {
        for input in state
            .keys
            .release_all()
            .into_iter()
            .chain(state.pointer.release_all())
        {
            let _ = viewer.input(None, &input);
        }
    }
    result
}

struct ClientState {
    pf: PixelFormat,
    supports_desktop_size: bool,
    pending_update: Option<bool>,
    last: Option<Frame>,
    keys: Keys,
    pointer: PointerState,
}

fn handle_client_msg(
    msg_type: u8,
    stream: &mut TcpStream,
    state: &mut ClientState,
    viewer: Option<&dyn Viewer>,
) -> Result<()> {
    match msg_type {
        0 => {
            let mut buf = [0u8; 19];
            read_exact_timeout(stream, &mut buf)?;
            state.pf = PixelFormat::from_bytes(buf[3..19].try_into().unwrap())?;
            state.last = None;
        }
        2 => {
            let mut head = [0u8; 3];
            read_exact_timeout(stream, &mut head)?;
            let mut raw = vec![0u8; u16::from_be_bytes([head[1], head[2]]) as usize * 4];
            read_exact_timeout(stream, &mut raw)?;
            state.supports_desktop_size = raw
                .as_chunks::<4>()
                .0
                .iter()
                .any(|b| i32::from_be_bytes(*b) == ENCODING_DESKTOP_SIZE);
        }
        3 => {
            let mut buf = [0u8; 9];
            read_exact_timeout(stream, &mut buf)?;
            let force_full = buf[0] == 0;
            if force_full {
                state.last = None;
            }
            state.pending_update = Some(state.pending_update.unwrap_or(false) || force_full);
        }
        4 => {
            let mut buf = [0u8; 7];
            read_exact_timeout(stream, &mut buf)?;
            let keysym = u32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]);
            if let Some(v) = viewer {
                for input in keysym_events(&mut state.keys, keysym, buf[0] != 0) {
                    let _ = v.input(None, &input);
                }
            }
        }
        5 => {
            let mut buf = [0u8; 5];
            read_exact_timeout(stream, &mut buf)?;
            let (x, y) = (
                u16::from_be_bytes([buf[1], buf[2]]) as f64,
                u16::from_be_bytes([buf[3], buf[4]]) as f64,
            );
            for input in state.pointer.event(buf[0], x, y) {
                if let Some(v) = viewer {
                    let _ = v.input(None, &input);
                }
            }
        }
        6 => {
            let mut head = [0u8; 7];
            read_exact_timeout(stream, &mut head)?;
            let len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
            ensure!(
                len <= MAX_CUT_TEXT,
                "ClientCutText length {len} exceeds {MAX_CUT_TEXT}"
            );
            let mut buf = vec![0u8; len];
            read_exact_timeout(stream, &mut buf)?;
            let text = match std::str::from_utf8(&buf) {
                Ok(s) => s.replace("\r\n", "\n"),
                Err(_) => buf
                    .iter()
                    .map(|&b| b as char)
                    .collect::<String>()
                    .replace("\r\n", "\n"),
            };
            if !text.is_empty() {
                if let Some(v) = viewer {
                    let _ = v.input(None, &ViewInput::Text { text });
                }
            }
        }
        other => bail!("unknown RFB client message type {other}"),
    }
    Ok(())
}

/// Parses the client's 12-byte `ProtocolVersion` banner (`RFB xxx.yyy\n`).
/// Accepts 3.8, Apple Screen Sharing's 3.889 (which speaks the 3.8
/// handshake), 3.7 and 3.3.
pub(crate) fn parse_version(banner: &[u8; 12]) -> Result<Protocol> {
    let s = std::str::from_utf8(banner).context("invalid RFB version banner")?;
    let Some(ver) = s.strip_prefix("RFB ").and_then(|s| s.strip_suffix('\n')) else {
        bail!("not an RFB version banner: {:?}", s.trim());
    };
    let Some((maj, min)) = ver.split_once('.') else {
        bail!("malformed RFB version: {ver}");
    };
    match (
        maj.trim().parse::<u32>().context("RFB major version")?,
        min.trim().parse::<u32>().context("RFB minor version")?,
    ) {
        (3, 8 | 889) => Ok(Protocol::Rfb38),
        (3, 7) => Ok(Protocol::Rfb37),
        (3, 3) => Ok(Protocol::Rfb33),
        (maj, min) => bail!("unsupported RFB version {maj}.{min}"),
    }
}

/// Tracks VNC pointer button mask and position across `PointerEvent` messages.
#[derive(Default)]
pub struct PointerState {
    mask: u8,
    pos: Option<(f64, f64)>,
}

impl PointerState {
    pub fn event(&mut self, mask: u8, x: f64, y: f64) -> Vec<ViewInput> {
        let mut out = Vec::new();
        let prev = self.mask;
        self.mask = mask;
        let moved = self.pos != Some((x, y));
        self.pos = Some((x, y));

        let buttons = [
            (1u8 << 0, MouseButton::Left),
            (1u8 << 1, MouseButton::Middle),
            (1u8 << 2, MouseButton::Right),
        ];
        let mut button_changed = false;
        for (bit, button) in buttons {
            let was = (prev & bit) != 0;
            let now = (mask & bit) != 0;
            if was != now {
                button_changed = true;
                out.push(ViewInput::Button {
                    x,
                    y,
                    button,
                    down: now,
                });
            }
        }
        if !button_changed && moved {
            out.push(ViewInput::Move { x, y });
        }

        // Wheel bits: 3 up, 4 down, 5 left, 6 right (act on press 0 -> 1).
        let pressed = mask & !prev;
        let mut dx = 0.0;
        let mut dy = 0.0;
        if (pressed & (1 << 3)) != 0 {
            dy -= 40.0;
        }
        if (pressed & (1 << 4)) != 0 {
            dy += 40.0;
        }
        if (pressed & (1 << 5)) != 0 {
            dx -= 40.0;
        }
        if (pressed & (1 << 6)) != 0 {
            dx += 40.0;
        }
        if dx != 0.0 || dy != 0.0 {
            out.push(ViewInput::Wheel { x, y, dx, dy });
        }
        out
    }

    pub fn release_all(&mut self) -> Vec<ViewInput> {
        let mut out = Vec::new();
        let (x, y) = self.pos.unwrap_or((0.0, 0.0));
        let buttons = [
            (1u8 << 0, MouseButton::Left),
            (1u8 << 1, MouseButton::Middle),
            (1u8 << 2, MouseButton::Right),
        ];
        for (bit, button) in buttons {
            if (self.mask & bit) != 0 {
                out.push(ViewInput::Button {
                    x,
                    y,
                    button,
                    down: false,
                });
            }
        }
        self.mask = 0;
        self.pos = None;
        out
    }
}

/// Translates an X11 keysym from a VNC `KeyEvent` into [`ViewInput`]s through
/// [`Keys`], sharing the WebSocket view's modifier and shortcut state machine.
pub fn keysym_events(keys: &mut Keys, keysym: u32, down: bool) -> Vec<ViewInput> {
    let action = if down { KeyAction::Down } else { KeyAction::Up };
    if let Some((code, key)) = keysym_to_dom(keysym) {
        return keys.event(action, code, &key);
    }
    if let Some(ch) = keysym_char(keysym) {
        let code = format!("Unicode{:04X}", ch as u32);
        return keys.event(action, &code, &ch.to_string());
    }
    Vec::new()
}

fn keysym_char(keysym: u32) -> Option<char> {
    if (0x20..=0x7e).contains(&keysym) || (0xa0..=0xff).contains(&keysym) {
        char::from_u32(keysym)
    } else if (keysym & 0xff00_0000) == 0x0100_0000 {
        char::from_u32(keysym & 0x00ff_ffff).filter(|c| !c.is_control())
    } else {
        None
    }
}

/// Maps an X11 keysym to its canonical unshifted DOM `(code, key)` pair.
fn keysym_to_dom(keysym: u32) -> Option<(&'static str, String)> {
    let named = |code: &'static str, key: &str| Some((code, key.to_string()));
    match keysym {
        0xff08 => return named("Backspace", "Backspace"),
        0xff09 | 0xfe20 => return named("Tab", "Tab"),
        0xff0d => return named("Enter", "Enter"),
        0xff13 => return named("Pause", "Pause"),
        0xff14 => return named("ScrollLock", "ScrollLock"),
        0xff1b => return named("Escape", "Escape"),
        0xff50 => return named("Home", "Home"),
        0xff51 => return named("ArrowLeft", "ArrowLeft"),
        0xff52 => return named("ArrowUp", "ArrowUp"),
        0xff53 => return named("ArrowRight", "ArrowRight"),
        0xff54 => return named("ArrowDown", "ArrowDown"),
        0xff55 => return named("PageUp", "PageUp"),
        0xff56 => return named("PageDown", "PageDown"),
        0xff57 => return named("End", "End"),
        0xff61 => return named("PrintScreen", "PrintScreen"),
        0xff63 => return named("Insert", "Insert"),
        0xff67 => return named("ContextMenu", "ContextMenu"),
        0xff7f => return named("NumLock", "NumLock"),
        0xff8d => return named("NumpadEnter", "Enter"),
        0xffaa => return named("NumpadMultiply", "*"),
        0xffab => return named("NumpadAdd", "+"),
        0xffad => return named("NumpadSubtract", "-"),
        0xffae => return named("NumpadDecimal", "."),
        0xffaf => return named("NumpadDivide", "/"),
        0xffb0..=0xffb9 => {
            const CODES: [&str; 10] = [
                "Numpad0", "Numpad1", "Numpad2", "Numpad3", "Numpad4", "Numpad5", "Numpad6",
                "Numpad7", "Numpad8", "Numpad9",
            ];
            let n = (keysym - 0xffb0) as usize;
            return Some((CODES[n], n.to_string()));
        }
        0xffbe..=0xffd5 => {
            const FCODES: [&str; 24] = [
                "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13",
                "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
            ];
            let idx = (keysym - 0xffbe) as usize;
            return Some((FCODES[idx], FCODES[idx].to_string()));
        }
        0xffe1 => return named("ShiftLeft", "Shift"),
        0xffe2 => return named("ShiftRight", "Shift"),
        0xffe3 => return named("ControlLeft", "Control"),
        0xffe4 => return named("ControlRight", "Control"),
        0xffe5 => return named("CapsLock", "CapsLock"),
        0xffe7 | 0xffeb => return named("MetaLeft", "Meta"),
        0xffe8 | 0xffec => return named("MetaRight", "Meta"),
        0xffe9 => return named("AltLeft", "Alt"),
        0xffea | 0xfe03 => return named("AltRight", "AltGraph"),
        0xffff => return named("Delete", "Delete"),
        _ => {}
    }

    if !(0x20..=0x7e).contains(&keysym) {
        return None;
    }
    let ch = char::from_u32(keysym)?;
    let key = ch.to_string();
    const LETTERS: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
        "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
        "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    let code = match ch {
        'a'..='z' => LETTERS[(ch as usize) - ('a' as usize)],
        'A'..='Z' => LETTERS[(ch as usize) - ('A' as usize)],
        '0' | ')' => "Digit0",
        '1' | '!' => "Digit1",
        '2' | '@' => "Digit2",
        '3' | '#' => "Digit3",
        '4' | '$' => "Digit4",
        '5' | '%' => "Digit5",
        '6' | '^' => "Digit6",
        '7' | '&' => "Digit7",
        '8' | '*' => "Digit8",
        '9' | '(' => "Digit9",
        '`' | '~' => "Backquote",
        '-' | '_' => "Minus",
        '=' | '+' => "Equal",
        '[' | '{' => "BracketLeft",
        ']' | '}' => "BracketRight",
        '\\' | '|' => "Backslash",
        ';' | ':' => "Semicolon",
        '\'' | '"' => "Quote",
        ',' | '<' => "Comma",
        '.' | '>' => "Period",
        '/' | '?' => "Slash",
        ' ' => "Space",
        _ => return None,
    };
    Some((code, key))
}

/// Encrypts a 16-byte VNC challenge with `password` per RFC 6143 §7.2.2.
pub fn vnc_encrypt_challenge(challenge: &[u8; 16], password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (i, &b) in password.iter().take(8).enumerate() {
        key[i] = b.reverse_bits();
    }
    let subkeys = des_subkeys(u64::from_be_bytes(key));
    let b0 = des_block(
        u64::from_be_bytes(challenge[0..8].try_into().unwrap()),
        &subkeys,
    );
    let b1 = des_block(
        u64::from_be_bytes(challenge[8..16].try_into().unwrap()),
        &subkeys,
    );
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&b0.to_be_bytes());
    out[8..16].copy_from_slice(&b1.to_be_bytes());
    out
}

#[rustfmt::skip]
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3,
    60, 52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37,
    29, 21, 13, 5, 28, 20, 12, 4,
];
#[rustfmt::skip]
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2,
    41, 52, 31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
#[rustfmt::skip]
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3,
    61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
#[rustfmt::skip]
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
#[rustfmt::skip]
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17,
    16, 17, 18, 19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
#[rustfmt::skip]
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10,
    2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4, 25,
];
#[rustfmt::skip]
const SBOX: [[[u8; 16]; 4]; 8] = [
    [[14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7], [0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8], [4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0], [15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13]],
    [[15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10], [3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5], [0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15], [13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9]],
    [[10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8], [13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1], [13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7], [1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12]],
    [[7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15], [13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9], [10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4], [3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14]],
    [[2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9], [14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6], [4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14], [11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3]],
    [[12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11], [10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8], [9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6], [4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13]],
    [[4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1], [13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6], [1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2], [6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12]],
    [[13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7], [1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2], [7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8], [2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11]],
];

fn permute(v: u64, in_bits: u8, table: &[u8]) -> u64 {
    let mut out = 0u64;
    for &pos in table {
        out = (out << 1) | ((v >> (in_bits - pos)) & 1);
    }
    out
}

fn des_subkeys(key: u64) -> [u64; 16] {
    let cd = permute(key, 64, &PC1);
    let mut c = ((cd >> 28) & 0x0fff_ffff) as u32;
    let mut d = (cd & 0x0fff_ffff) as u32;
    let rot28 = |x: u32, n: u8| ((x << n) | (x >> (28 - n))) & 0x0fff_ffff;
    let mut subkeys = [0u64; 16];
    for (i, &s) in SHIFTS.iter().enumerate() {
        c = rot28(c, s);
        d = rot28(d, s);
        let merged = ((c as u64) << 28) | (d as u64);
        subkeys[i] = permute(merged, 56, &PC2);
    }
    subkeys
}

fn des_block(block: u64, subkeys: &[u64; 16]) -> u64 {
    let ip = permute(block, 64, &IP);
    let mut l = (ip >> 32) as u32;
    let mut r = ip as u32;
    for &k in subkeys {
        let expanded = permute(r as u64, 32, &E) ^ k;
        let mut s_out = 0u32;
        for (i, sbox) in SBOX.iter().enumerate() {
            let six = ((expanded >> (42 - i * 6)) & 0x3f) as usize;
            let row = ((six & 0x20) >> 4) | (six & 0x01);
            let col = (six >> 1) & 0x0f;
            s_out = (s_out << 4) | (sbox[row][col] as u32);
        }
        let f = permute(s_out as u64, 32, &P) as u32;
        (l, r) = (r, l ^ f);
    }
    let pre = ((r as u64) << 32) | (l as u64);
    permute(pre, 64, &FP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn des_fips_and_vnc_bit_reversal() {
        let subkeys = des_subkeys(0x1334_5779_9bbc_dff1);
        assert_eq!(
            des_block(0x0123_4567_89ab_cdef, &subkeys),
            0x85e8_1354_0f0a_b405
        );
        let raw_key = 0x1334_5779_9bbc_dff1u64.to_be_bytes();
        let vnc_pw: Vec<u8> = raw_key.iter().map(|b| b.reverse_bits()).collect();
        let challenge: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef,
        ];
        let enc = vnc_encrypt_challenge(&challenge, &vnc_pw);
        assert_eq!(&enc[0..8], &0x85e8_1354_0f0a_b405u64.to_be_bytes());
        assert_eq!(&enc[8..16], &0x85e8_1354_0f0a_b405u64.to_be_bytes());
    }

    #[test]
    fn pixel_formats_pack_bgra_and_rgba() {
        let frame = Frame {
            window_id: 1,
            width: 2,
            height: 1,
            stride: 8,
            data: vec![255, 0, 0, 255, 0, 255, 128, 255],
            order: Order::Rgba,
        };
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        let raw32 = encode_raw(&frame, region, PixelFormat::default());
        assert_eq!(raw32, vec![0, 0, 255, 0, 128, 255, 0, 0]);

        let rgb565 = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: true,
            true_color: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        let raw16 = encode_raw(&frame, region, rgb565);
        assert_eq!(&raw16[0..2], &(31u16 << 11).to_be_bytes());

        let mut bad_tc = rgb565.to_bytes();
        bad_tc[3] = 0;
        assert!(PixelFormat::from_bytes(&bad_tc).is_err());
        let mut bad_max = rgb565.to_bytes();
        bad_max[4] = 0;
        bad_max[5] = 0;
        assert!(PixelFormat::from_bytes(&bad_max).is_err());
    }

    #[test]
    fn session_sequence_sorts_numerically() {
        assert!(session_seq("s10-abcdef") > session_seq("s2-abcdef"));
        assert_eq!(parse_version(b"RFB 003.889\n").unwrap(), Protocol::Rfb38);
    }

    #[test]
    fn pointer_buttons_and_wheel_transitions() {
        let mut p = PointerState::default();
        assert_eq!(
            p.event(0, 10.0, 20.0),
            vec![ViewInput::Move { x: 10.0, y: 20.0 }]
        );
        assert_eq!(
            p.event(1, 10.0, 20.0),
            vec![ViewInput::Button {
                x: 10.0,
                y: 20.0,
                button: MouseButton::Left,
                down: true,
            }]
        );
        assert_eq!(
            p.event(0, 10.0, 20.0),
            vec![ViewInput::Button {
                x: 10.0,
                y: 20.0,
                button: MouseButton::Left,
                down: false,
            }]
        );
        assert_eq!(
            p.event(1 << 4, 10.0, 20.0),
            vec![ViewInput::Wheel {
                x: 10.0,
                y: 20.0,
                dx: 0.0,
                dy: 40.0,
            }]
        );
        assert!(p.event(0, 10.0, 20.0).is_empty());
    }

    #[test]
    fn keysyms_type_characters_and_press_shortcuts() {
        let mut k = Keys::default();
        assert_eq!(
            keysym_events(&mut k, 'a' as u32, true),
            vec![ViewInput::Text { text: "a".into() }]
        );
        assert!(keysym_events(&mut k, 'a' as u32, false).is_empty());
        assert_eq!(
            keysym_events(&mut k, 0xffe1, true),
            vec![ViewInput::Key {
                code: "ShiftLeft".into(),
                down: true,
            }]
        );
        assert_eq!(
            keysym_events(&mut k, 'A' as u32, true),
            vec![ViewInput::Text { text: "A".into() }]
        );
        assert_eq!(
            keysym_events(&mut k, 0xffe1, false),
            vec![ViewInput::Key {
                code: "ShiftLeft".into(),
                down: false,
            }]
        );
        assert!(keysym_events(&mut k, 'a' as u32, false).is_empty());

        keysym_events(&mut k, 0xffe3, true);
        assert_eq!(
            keysym_events(&mut k, 'c' as u32, true),
            vec![ViewInput::Key {
                code: "KeyC".into(),
                down: true,
            }]
        );
        assert_eq!(
            keysym_events(&mut k, 'c' as u32, false),
            vec![ViewInput::Key {
                code: "KeyC".into(),
                down: false,
            }]
        );
        assert_eq!(k.release_all().len(), 1);
    }

    #[test]
    fn rfb_handshake_frames_and_input_roundtrip() {
        struct MockViewer {
            inputs: Mutex<Vec<ViewInput>>,
        }
        impl Viewer for MockViewer {
            fn windows(&self) -> Result<Vec<ocu_core::WindowInfo>> {
                Ok(vec![ocu_core::WindowInfo {
                    id: 1,
                    title: "Test".into(),
                    frame: ocu_core::Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 4.0,
                        height: 2.0,
                    },
                    on_screen: true,
                }])
            }
            fn frame(&self, _: Option<u64>) -> Result<Frame> {
                Ok(Frame {
                    window_id: 1,
                    width: 4,
                    height: 2,
                    stride: 16,
                    data: vec![0x42; 32],
                    order: Order::Bgra,
                })
            }
            fn input(&self, _: Option<u64>, input: &ViewInput) -> Result<()> {
                self.inputs.lock().unwrap().push(input.clone());
                Ok(())
            }
            fn is_alive(&self) -> bool {
                true
            }
        }
        struct MockSession(Arc<MockViewer>);
        impl ocu_core::Session for MockSession {
            fn describe(&self) -> ocu_core::Description {
                ocu_core::Description {
                    app: "TestApp".into(),
                    pid: Some(42),
                    ..Default::default()
                }
            }
            fn windows(&mut self) -> Result<Vec<ocu_core::WindowInfo>> {
                self.0.windows()
            }
            fn screenshot(&mut self, _: Option<u64>) -> Result<ocu_core::Screenshot> {
                bail!("unused")
            }
            fn perform(&mut self, _: Option<u64>, _: &ocu_core::Action) -> Result<()> {
                Ok(())
            }
            fn is_alive(&mut self) -> bool {
                true
            }
            fn close(&mut self) {}
            fn viewer(&mut self) -> Option<Arc<dyn Viewer>> {
                Some(self.0.clone())
            }
        }
        struct MockPlatform(Arc<MockViewer>);
        impl ocu_core::Platform for MockPlatform {
            fn name(&self) -> &'static str {
                "linux-wayland"
            }
            fn launch(&self, _: &ocu_core::LaunchSpec) -> Result<Box<dyn ocu_core::Session>> {
                Ok(Box::new(MockSession(self.0.clone())))
            }
        }

        let viewer = Arc::new(MockViewer {
            inputs: Mutex::new(Vec::new()),
        });
        let service = Service::new(Arc::new(MockPlatform(viewer.clone())), None);
        service.share_sessions();
        let mut client_handle = service.client();
        ocu_core::Handler::handle(
            &mut client_handle,
            ocu_core::Request::StartSession(ocu_core::LaunchSpec {
                app: "TestApp".into(),
                ..Default::default()
            }),
        )
        .unwrap();

        let token = "secret_token_123456";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = service.clone();
        let tok = token.to_string();
        let handle = std::thread::spawn(move || {
            let (srv, _) = listener.accept().unwrap();
            connection(&svc, &tok, srv)
        });

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut greeting = [0u8; 12];
        c.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, b"RFB 003.008\n");
        c.write_all(b"RFB 003.889\n").unwrap();

        let mut sec = [0u8; 2];
        c.read_exact(&mut sec).unwrap();
        assert_eq!(sec, [1, 2]);
        c.write_all(&[2]).unwrap();

        let mut challenge = [0u8; 16];
        c.read_exact(&mut challenge).unwrap();
        let resp = vnc_encrypt_challenge(&challenge, token.as_bytes());
        c.write_all(&resp).unwrap();

        let mut res = [0u8; 4];
        c.read_exact(&mut res).unwrap();
        assert_eq!(u32::from_be_bytes(res), 0);

        c.write_all(&[1]).unwrap();
        let mut init = [0u8; 24];
        c.read_exact(&mut init).unwrap();
        assert_eq!(u16::from_be_bytes([init[0], init[1]]), 4);
        assert_eq!(u16::from_be_bytes([init[2], init[3]]), 2);
        let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
        let mut name = vec![0u8; name_len];
        c.read_exact(&mut name).unwrap();
        assert!(String::from_utf8_lossy(&name).starts_with("TestApp ("));

        c.write_all(&[3, 0, 0, 0, 0, 0, 0, 4, 0, 2]).unwrap();
        c.write_all(&[5, 1, 0, 2, 0, 1]).unwrap();
        c.write_all(&[4, 1, 0, 0, 0, 0, 0, b'x']).unwrap();

        let mut fb_head = [0u8; 16];
        c.read_exact(&mut fb_head).unwrap();
        assert_eq!(fb_head[0], 0);
        assert_eq!(u16::from_be_bytes([fb_head[2], fb_head[3]]), 1);
        assert_eq!(u16::from_be_bytes([fb_head[8], fb_head[9]]), 4);
        assert_eq!(u16::from_be_bytes([fb_head[10], fb_head[11]]), 2);
        let mut pixels = [0u8; 32];
        c.read_exact(&mut pixels).unwrap();
        assert_eq!(pixels, [0x42; 32]);

        drop(c);
        handle.join().unwrap().unwrap();

        let got = viewer.inputs.lock().unwrap().clone();
        assert!(got.contains(&ViewInput::Button {
            x: 2.0,
            y: 1.0,
            button: MouseButton::Left,
            down: true,
        }));
        assert!(got.contains(&ViewInput::Text { text: "x".into() }));
    }
}
