//! Window pictures for the live view, streamed by ScreenCaptureKit's
//! `SCStream` as the window changes, rather than one screenshot at a time.

use std::ffi::c_void;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread as _, DefinedClass as _};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};

use ocu_core::image::Order;
use ocu_core::{Capturer, Frame, Latest, Stream, WindowInfo};

use crate::{capture, panel};

/// Streams an app's windows.
pub struct Streamer {
    pub pid: i32,
}

impl Capturer for Streamer {
    fn stream(&self, window: &WindowInfo) -> Result<Stream> {
        // A panel's host window only shows the panel composed with the
        // screen, which a screenshot does.
        if panel::remotes(&capture::windows(self.pid)).contains_key(&window.id) {
            bail!(
                "window {} hosts a panel; its pictures are composed",
                window.id
            );
        }
        let content = capture::shareable_content()?;
        let sc_window = capture::find_window(&content, window.id)?;
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &sc_window)
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            // Points, as screenshots are, so pixels and coordinates agree.
            config.setWidth(window.frame.width.round().max(1.0) as usize);
            config.setHeight(window.frame.height.round().max(1.0) as usize);
            config.setIgnoreShadowsSingleWindow(true);
            config.setShowsCursor(false);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setMinimumFrameInterval(CMTime::new(1, 30));
            config.setQueueDepth(3);
        }
        let latest = Latest::new();
        let output = Output::new(latest.clone(), window.id);
        let queue = DispatchQueue::new("opencomputeruse.view", None);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| anyhow!("{}", e.localizedDescription()))?;
        let (tx, rx) = mpsc::channel();
        let started = RcBlock::new(move |error: *mut NSError| {
            let _ =
                tx.send(unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()));
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(None) => {}
            Ok(Some(e)) => bail!(
                "ScreenCaptureKit would not stream window {}: {e}",
                window.id
            ),
            Err(_) => bail!(
                "ScreenCaptureKit did not start streaming window {}",
                window.id
            ),
        }
        Ok(Stream::new(
            latest,
            Running {
                stream,
                _output: output,
                _queue: queue,
            },
        ))
    }
}

/// A started stream, stopped when dropped.
struct Running {
    stream: Retained<SCStream>,
    /// The stream holds its output and delegate weakly.
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
}

// ScreenCaptureKit's objects may be used from any thread.
unsafe impl Send for Running {}
unsafe impl Sync for Running {}

impl Drop for Running {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

struct Ivars {
    latest: Arc<Latest>,
    window: u64,
}

define_class!(
    /// Takes a stream's pictures as they come, on its dispatch queue.
    #[unsafe(super(NSObject))]
    #[name = "OcuViewStreamOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen || !complete(buffer) {
                return;
            }
            if let Some(frame) = frame(buffer, self.ivars().window) {
                self.ivars().latest.push(frame);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            log::info!(
                "streaming window {} stopped: {}",
                self.ivars().window,
                error.localizedDescription()
            );
            self.ivars().latest.stop();
        }
    }
);

impl Output {
    fn new(latest: Arc<Latest>, window: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { latest, window });
        unsafe { msg_send![super(this), init] }
    }
}

/// Whether a sample holds a new picture. The stream also sends ones that
/// only say nothing changed, and for a single window, complete ones at its
/// full rate whose dirty rects say nothing changed either.
fn complete(buffer: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { buffer.sample_attachments_array(false) }) else {
        return false;
    };
    let attachments: &CFArray<CFDictionary> = unsafe { attachments.cast_unchecked() };
    let Some(info) = attachments.iter().next() else {
        return false;
    };
    let value = |key: &NSString| {
        let v = unsafe { info.value((key as *const NSString).cast::<c_void>()) };
        (!v.is_null()).then_some(v)
    };
    let Some(status) = value(unsafe { SCStreamFrameInfoStatus }) else {
        return false;
    };
    let status = unsafe { &*(status as *const CFNumber) };
    if status.as_i64() != Some(SCFrameStatus::Complete.0 as i64) {
        return false;
    }
    // No dirty rects listed at all is a picture to take; an empty list is
    // the same picture again.
    match value(unsafe { SCStreamFrameInfoDirtyRects }) {
        Some(rects) => unsafe { &*(rects as *const CFArray) }.count() > 0,
        None => true,
    }
}

/// A sample's pixels, copied out of its buffer.
fn frame(buffer: &CMSampleBuffer, window: u64) -> Option<Frame> {
    let pixels = unsafe { buffer.image_buffer() }?;
    if CVPixelBufferGetPixelFormatType(&pixels) != kCVPixelFormatType_32BGRA {
        return None;
    }
    if unsafe { CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return None;
    }
    let (width, height) = (
        CVPixelBufferGetWidth(&pixels),
        CVPixelBufferGetHeight(&pixels),
    );
    let stride = CVPixelBufferGetBytesPerRow(&pixels);
    let base = CVPixelBufferGetBaseAddress(&pixels) as *const u8;
    let data = (!base.is_null())
        .then(|| unsafe { std::slice::from_raw_parts(base, stride * height) }.to_vec());
    unsafe { CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) };
    Some(Frame {
        window_id: window,
        width: width as u32,
        height: height as u32,
        stride,
        data: data?,
        order: Order::Bgra,
    })
}
