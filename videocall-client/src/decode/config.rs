use crate::constants::AUDIO_CHANNELS;
use crate::constants::AUDIO_SAMPLE_RATE;
use js_sys::Array;
use log::{info, warn, debug};
use wasm_bindgen::JsValue;
use wasm_bindgen::JsCast;
use web_sys::{AudioContext, AudioContextOptions};
use web_sys::{MediaStream, MediaStreamTrack};

// Helper function to get JavaScript typeof
fn js_typeof(val: &JsValue) -> String {
    let window = web_sys::window().expect("no global window exists");
    let typeof_fn = r#"
    function(obj) {
        return typeof obj;
    }
    "#;
    
    match js_sys::Function::new_with_args("obj", typeof_fn).call1(&JsValue::NULL, val) {
        Ok(js_type) => js_type.as_string().unwrap_or_else(|| "unknown".to_string()),
        Err(_) => "error".to_string(),
    }
}

pub fn configure_audio_context(audio_track_value: &JsValue) -> anyhow::Result<AudioContext> {
    info!(
        "Configuring audio context with sample rate: {}",
        AUDIO_SAMPLE_RATE
    );
    debug!("Audio track type: {:?}", js_typeof(audio_track_value));

    // Declare media_stream variable to use throughout the function
    let media_stream: MediaStream;

    // First, try to create a MediaStream directly using JS to handle edge cases
    let window = web_sys::window().expect("no global window exists");
    let create_stream_fn = js_sys::Function::new_with_args(
        "track", 
        r#"
        try {
            console.log("Attempting to create MediaStream directly", track);
            const stream = new MediaStream([track]);
            return { success: true, stream: stream };
        } catch (e) {
            console.error("Failed to create MediaStream:", e);
            return { success: false, error: e.message };
        }
        "#
    );
    
    let result = match create_stream_fn.call1(&window, audio_track_value) {
        Ok(res) => res,
        Err(e) => {
            warn!("Failed to call JS function: {:?}", e);
            return Err(anyhow::anyhow!("JavaScript error: {}", e.as_string().unwrap_or_default()));
        }
    };
    
    // Check if the JS-side creation was successful
    let result_obj = js_sys::Object::from(result.clone());
    let success = match js_sys::Reflect::get(&result_obj, &JsValue::from_str("success")) {
        Ok(val) => val.as_bool().unwrap_or(false),
        Err(_) => false,
    };
    
    if success {
        debug!("Successfully created MediaStream in JavaScript");
        let js_stream = match js_sys::Reflect::get(&result_obj, &JsValue::from_str("stream")) {
            Ok(stream) => stream,
            Err(e) => {
                warn!("Failed to get stream from result: {:?}", e);
                return Err(anyhow::anyhow!("Failed to get stream from result"));
            }
        };
        
        // Convert JS MediaStream to web_sys::MediaStream
        media_stream = match js_stream.dyn_into::<MediaStream>() {
            Ok(stream) => stream,
            Err(e) => {
                warn!("Failed to convert JS MediaStream: {:?}", e);
                return Err(anyhow::anyhow!("Failed to convert JS MediaStream"));
            }
        };
        
        info!("Created media stream with audio track");
    } else {
        // Fall back to our original approach
        debug!("Falling back to Rust-side approach");
        
        let js_tracks = Array::new();
        
        // First try to cast the JsValue to MediaStreamTrack
        match audio_track_value.dyn_ref::<MediaStreamTrack>() {
            Some(track) => {
                info!("Successfully cast JsValue to MediaStreamTrack");
                debug!("Track kind: {}", track.kind());
                js_tracks.push(track);
            }
            None => {
                // If casting fails, try to use it directly
                warn!("Could not cast to MediaStreamTrack, using JsValue directly");
                
                // Try to get more information about the object for debugging
                let debug_obj = js_sys::Function::new_with_args(
                    "obj", 
                    r#"
                    try {
                        return {
                            toString: obj.toString ? obj.toString() : "no toString",
                            hasKind: obj.kind !== undefined,
                            kind: obj.kind,
                            prototype: obj.__proto__ ? obj.__proto__.constructor.name : "unknown",
                            methods: Object.getOwnPropertyNames(obj)
                        };
                    } catch (e) {
                        return { error: e.message };
                    }
                    "#
                );
                
                if let Ok(debug_info) = debug_obj.call1(&window, audio_track_value) {
                    debug!("Track debug info: {:?}", debug_info);
                }
                
                js_tracks.push(audio_track_value);
            }
        }
        
        media_stream = match MediaStream::new_with_tracks(&js_tracks) {
            Ok(stream) => stream,
            Err(e) => {
                warn!("Failed to create media stream: {:?}", e);
                
                // Try to get more info about the error
                let debug_err = js_sys::Function::new_with_args(
                    "error", 
                    r#"
                    try {
                        return {
                            toString: error.toString ? error.toString() : "no toString",
                            name: error.name,
                            message: error.message,
                            stack: error.stack
                        };
                    } catch (e) {
                        return { meta_error: e.message };
                    }
                    "#
                );
                
                let error_info = match debug_err.call1(&window, &e) {
                    Ok(info) => format!("{:?}", info),
                    Err(_) => "Unable to extract error details".to_string(),
                };
                
                return Err(anyhow::anyhow!("Failed to create media stream: {}", error_info));
            }
        };
        
        info!("Created media stream with audio track");
    }

    let audio_context_options = AudioContextOptions::new();
    audio_context_options.set_sample_rate(AUDIO_SAMPLE_RATE as f32);
    let audio_context = AudioContext::new_with_context_options(&audio_context_options).unwrap();
    info!("Created audio context");

    // Create gain node for volume control
    let gain_node = match audio_context.create_gain() {
        Ok(node) => node,
        Err(e) => return Err(anyhow::anyhow!("Failed to create gain node: {:?}", e)),
    };
    
    gain_node.gain().set_value(1.0);
    gain_node.set_channel_count(AUDIO_CHANNELS);
    info!("Created gain node with {} channels", AUDIO_CHANNELS);

    // Create media stream source
    let source = match audio_context.create_media_stream_source(&media_stream) {
        Ok(src) => src,
        Err(e) => return Err(anyhow::anyhow!("Failed to create media stream source: {:?}", e)),
    };
    
    info!("Created media stream source");

    // Connect nodes: source -> gain -> destination
    if let Err(e) = source.connect_with_audio_node(&gain_node) {
        return Err(anyhow::anyhow!("Failed to connect source to gain node: {:?}", e));
    }
    
    if let Err(e) = gain_node.connect_with_audio_node(&audio_context.destination()) {
        return Err(anyhow::anyhow!("Failed to connect gain node to destination: {:?}", e));
    }
    
    info!("Connected audio nodes: source -> gain -> destination");

    Ok(audio_context)
}
