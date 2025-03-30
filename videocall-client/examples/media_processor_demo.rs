//! MediaFrameProcessor Example
//!
//! This example demonstrates how to use the MediaFrameProcessor to access
//! camera frames in a browser environment in a cross-browser compatible way.
//!
//! To run this example:
//! ```
//! wasm-pack build --target web --dev
//! python -m http.server
//! # Open browser at http://localhost:8000
//! ```

use std::rc::Rc;
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    console,
    CanvasRenderingContext2d, HtmlCanvasElement, HtmlVideoElement, MediaStream,
    MediaStreamConstraints, MediaStreamTrack, VideoFrame,
};

use videocall_client::media_processor::MediaFrameProcessor;
use videocall_client::media_processor::MediaProcessorDemo;

// Keep track of how many frames we've processed
static FRAME_COUNT: AtomicUsize = AtomicUsize::new(0);

// Rust binary entry point - this gets called when running the example as a binary
fn main() {
    println!("This example demonstrates the MediaFrameProcessor");
    println!("To run this example:");
    println!("1. Build with: wasm-pack build --target web --dev");
    println!("2. Serve the files: python -m http.server");
    println!("3. Open in browser: http://localhost:8000/examples/media_processor_demo.html");
}

/// Entry point for the example when compiled to WebAssembly
#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    // Set up panic hook for better error messages
    console_error_panic_hook::set_once();
    
    // Log the starting message
    web_sys::console::log_1(&JsValue::from_str("Starting MediaFrameProcessor example"));
    
    // Start the example
    spawn_local(async {
        match run_example().await {
            Ok(_) => {
                web_sys::console::log_1(&JsValue::from_str("Example is running"));
            }
            Err(e) => {
                web_sys::console::error_1(&e);
            }
        }
    });
    
    Ok(())
}

/// Main example function
async fn run_example() -> Result<(), JsValue> {
    // Get DOM elements or create them
    let document = web_sys::window()
        .ok_or_else(|| JsValue::from_str("No window found"))?
        .document()
        .ok_or_else(|| JsValue::from_str("No document found"))?;
    
    // Create container
    let container = document.create_element("div")?;
    container.set_id("example-container");
    container.set_attribute("style", "display: flex; flex-direction: column; gap: 10px; max-width: 800px; margin: 0 auto;")?;
    
    // Create header
    let header = document.create_element("h1")?;
    header.set_text_content(Some("MediaFrameProcessor Example"));
    container.append_child(&header)?;
    
    // Create description
    let description = document.create_element("p")?;
    description.set_text_content(Some("This example demonstrates the MediaFrameProcessor cross-browser abstraction, which works in Safari and other browsers."));
    container.append_child(&description)?;
    
    // Create video element
    let video = document.create_element("video")?;
    video.set_id("example-video");
    video.set_attribute("autoplay", "true")?;
    video.set_attribute("muted", "true")?;
    video.set_attribute("playsinline", "true")?;
    video.set_attribute("style", "width: 100%; max-width: 640px; margin: 0 auto; border: 1px solid #ccc;")?;
    container.append_child(&video)?;
    
    // Create canvas element
    let canvas = document.create_element("canvas")?;
    canvas.set_id("example-canvas");
    canvas.set_attribute("width", "640")?;
    canvas.set_attribute("height", "480")?;
    canvas.set_attribute("style", "width: 100%; max-width: 640px; margin: 0 auto; border: 1px solid #ccc;")?;
    container.append_child(&canvas)?;
    
    // Create stats div
    let stats = document.create_element("div")?;
    stats.set_id("example-stats");
    stats.set_attribute("style", "font-family: monospace; background: #f0f0f0; padding: 10px; border-radius: 4px;")?;
    stats.set_text_content(Some("Initializing..."));
    container.append_child(&stats)?;
    
    // Add container to body
    document.body()
        .ok_or_else(|| JsValue::from_str("No body found"))?
        .append_child(&container)?;
    
    // Get video and canvas elements with proper types
    let video_elem = video.dyn_into::<HtmlVideoElement>()?;
    let canvas_elem = canvas.dyn_into::<HtmlCanvasElement>()?;
    let ctx = canvas_elem
        .get_context("2d")?
        .ok_or_else(|| JsValue::from_str("Failed to get 2d context"))?
        .dyn_into::<CanvasRenderingContext2d>()?;
    
    // Get user media
    let media_stream = get_user_media().await?;
    
    // Set video source
    video_elem.set_src_object(Some(&media_stream));
    
    // Get video track
    let video_tracks = media_stream.get_video_tracks();
    if video_tracks.length() == 0 {
        return Err(JsValue::from_str("No video tracks found"));
    }
    
    let video_track = video_tracks.get(0).dyn_into::<MediaStreamTrack>()?;
    
    // Show track info
    let track_settings = video_track.get_settings();
    web_sys::console::log_1(&JsValue::from_str(&format!(
        "Video track: {}x{} @ {}fps",
        track_settings.get_width().unwrap_or(0),
        track_settings.get_height().unwrap_or(0),
        track_settings.get_frame_rate().unwrap_or(0.0)
    )));
    
    // Create MediaFrameProcessor
    let processor = MediaFrameProcessor::new(&video_track)?;
    
    // Store processor and context in Rc<RefCell<>> for the frame processing loop
    let processor_rc = Rc::new(RefCell::new(processor));
    let ctx_rc = Rc::new(RefCell::new(ctx));
    let stats_rc = Rc::new(RefCell::new(stats));
    
    // Set up periodic stats update
    let stats_processor_rc = processor_rc.clone();
    let stats_updater = Closure::wrap(Box::new(move || {
        let processor = stats_processor_rc.borrow();
        let stats_elem = stats_rc.borrow();
        let frame_count = FRAME_COUNT.load(Ordering::Relaxed);
        
        stats_elem.set_text_content(Some(&format!(
            "Processor type: {}\nTrack kind: {}\nFrames processed: {}",
            if MediaFrameProcessor::is_supported() { "Native" } else { "Polyfill" },
            processor.track_kind(),
            frame_count
        )));
    }) as Box<dyn FnMut()>);
    
    let window = web_sys::window().unwrap();
    let _interval_id = window.set_interval_with_callback_and_timeout_and_arguments_0(
        stats_updater.as_ref().unchecked_ref(),
        1000,
    )?;
    
    // Keep the closure alive
    stats_updater.forget();
    
    // Process frames in a loop
    process_frames(processor_rc, ctx_rc).await?;
    
    Ok(())
}

/// Gets access to the user's camera
async fn get_user_media() -> Result<MediaStream, JsValue> {
    let window = web_sys::window().expect("no global window exists");
    let navigator = window.navigator();
    let media_devices = navigator.media_devices()?;
    
    // Request video only
    let mut constraints = MediaStreamConstraints::new();
    constraints.video(&JsValue::TRUE);
    constraints.audio(&JsValue::FALSE);
    
    // Get user media
    let promise = media_devices.get_user_media_with_constraints(&constraints)?;
    let stream = wasm_bindgen_futures::JsFuture::from(promise).await?;
    
    Ok(stream.dyn_into::<MediaStream>()?)
}

/// Main processing loop for video frames
async fn process_frames(
    processor_rc: Rc<RefCell<MediaFrameProcessor>>,
    ctx_rc: Rc<RefCell<CanvasRenderingContext2d>>,
) -> Result<(), JsValue> {
    let processor = processor_rc.borrow();
    
    // Process frames using the lower-level API directly
    processor.process_video_frames(|frame| {
        let count = FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
        let ctx = ctx_rc.borrow();
        
        // Draw frame to canvas
        if count % 5 == 0 {  // Only process every 5th frame for better performance
            // Clear canvas
            ctx.clear_rect(0.0, 0.0, 640.0, 480.0);
            
            // Draw frame
            ctx.draw_image_with_video_frame(&frame, 0.0, 0.0)?;
            
            // Add some text
            ctx.set_fill_style(&JsValue::from_str("white"));
            ctx.set_font("14px monospace");
            ctx.fill_text(
                &format!("Frame #{}: {}x{}", count, frame.display_width(), frame.display_height()),
                10.0,
                20.0,
            )?;
        }
        
        // Don't forget to close the frame!
        frame.close();
        
        Ok(())
    }).await
}

#[wasm_bindgen]
pub async fn start(container_id: &str) -> Result<(), JsValue> {
    let mut demo = MediaProcessorDemo::new(container_id.to_string());
    demo.start().await
}

#[wasm_bindgen]
pub fn stop(container_id: &str) -> Result<(), JsValue> {
    let window = web_sys::window().expect("no global window exists");
    let document = window.document().expect("no document exists");
    
    // Clear container
    if let Some(container) = document.get_element_by_id(container_id) {
        container.set_inner_html("");
    }
    
    console::log_1(&JsValue::from_str("MediaFrameProcessor demo stopped"));
    Ok(())
} 