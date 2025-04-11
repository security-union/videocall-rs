// yew-ui/src/components/host.rs
use crate::components::device_selector::DeviceSelector;
use crate::constants::*; // Assuming bitrate constants are defined here now
use crate::utils::is_ios; // Assuming utils module exists at crate root
use futures::channel::mpsc::{self, UnboundedReceiver}; // For MPSC channel
use gloo_timers::callback::Timeout;
use log::{debug, error, info}; // Use info/error where appropriate
use std::fmt::{self, Debug, Display};
use videocall_client::{
    // Import concrete types for instantiation
    CameraEncoder,
    MicrophoneEncoder,
    ScreenEncoder,
    VideoCallClient,
    // Import the new iOS encoder
    IosCameraEncoder,
    // Assume EncoderControlMessage enum/struct is defined in videocall-client
    // E.g., pub enum EncoderControlMessage { SetBitrate(u32), RequestKeyframe }
    EncoderControlMessage,
};
use videocall_types::protos::media_packet::media_packet::MediaType;
use yew::prelude::*;

// --- Constants ---
const VIDEO_ELEMENT_ID: &str = "webcam";
// Define placeholder bitrate constants if not in constants.rs
// TODO: Define these properly in constants.rs
const VIDEO_BITRATE_KBPS: u32 = 1000; // Example: 1 Mbps
const AUDIO_BITRATE_KBPS: u32 = 64; // Example: 64 Kbps (Opus)
const SCREEN_BITRATE_KBPS: u32 = 2000; // Example: 2 Mbps

// --- Trait Definitions (Should potentially live in videocall-client or a shared place) ---

/// Common trait for Camera Encoders (Standard and iOS)
pub trait CameraEncoderTrait: Debug {
    // `new` typically can't be part of the trait easily for Box<dyn> construction.
    // Use factory functions or separate initialization if needed outside `Host::create`.
    // Methods called by Host:
    fn set_enabled(&mut self, value: bool) -> bool;
    fn select(&mut self, device_id: String) -> bool;
    fn start(&mut self);
    fn stop(&mut self);
    /// Sets the receiver for control messages (e.g., bitrate changes)
    fn set_encoder_control(&mut self, rx: UnboundedReceiver<EncoderControlMessage>);
    // Add any other methods Host needs to call directly
}

// Assume MicrophoneEncoderTrait and ScreenEncoderTrait are defined similarly if needed

// --- Implement Trait for Existing Encoders (in videocall-client) ---
// Example: Assuming implementations exist within videocall-client crate
/*
impl CameraEncoderTrait for CameraEncoder {
    // ... implementations matching the trait methods ...
    fn set_encoder_control(&mut self, rx: UnboundedReceiver<EncoderControlMessage>) {
        // Existing CameraEncoder needs logic to handle received messages
        self.internal_control_rx = Some(rx);
        // Spawn a task or integrate into existing loop to poll rx
        self.spawn_control_listener(); // Hypothetical method
        info!("Standard CameraEncoder: Control channel set.");
    }
}

impl CameraEncoderTrait for IosCameraEncoder {
    // ... implementations matching the trait methods ...
    fn set_encoder_control(&mut self, rx: UnboundedReceiver<EncoderControlMessage>) {
        // IosCameraEncoder needs to receive messages and forward them to the worker
        self.internal_control_rx = Some(rx);
        // Spawn a task or integrate into main thread loop to poll rx
        self.spawn_control_listener(); // Hypothetical method
        info!("iOS CameraEncoder: Control channel set.");
    }
}
*/

// --- Host Component ---

#[derive(Debug)]
pub enum Msg {
    Start,
    EnableScreenShare,
    DisableScreenShare,
    EnableMicrophone(bool),
    DisableMicrophone,
    EnableVideo(bool),
    DisableVideo,
    AudioDeviceChanged(String),
    VideoDeviceChanged(String),
    // Receive settings string from encoder's callback
    CameraEncoderSettingsUpdated(String),
    MicrophoneEncoderSettingsUpdated(String),
    ScreenEncoderSettingsUpdated(String),
}

// Holds diagnostic info from encoders
#[derive(Default, Debug, Clone, PartialEq)]
pub struct EncoderSettings {
    pub camera: Option<String>,
    pub microphone: Option<String>,
    pub screen: Option<String>,
}

impl Display for EncoderSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Improved formatting, handles None more gracefully
        writeln!(f, "Camera: {}", self.camera.as_deref().unwrap_or("N/A"))?;
        writeln!(f, "Mic:    {}", self.microphone.as_deref().unwrap_or("N/A"))?;
        write!(f, "Screen: {}", self.screen.as_deref().unwrap_or("N/A"))
    }
}

pub struct Host {
    // Use Trait Objects to store either standard or iOS encoder
    // TODO: Define and implement MicrophoneEncoderTrait and ScreenEncoderTrait if needed
    pub camera: Box<dyn CameraEncoderTrait>,
    pub microphone: MicrophoneEncoder, // Keep concrete for now, assume no iOS issue
    pub screen: ScreenEncoder,       // Keep concrete for now
    // State flags
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    // State for diagnostic display
    pub encoder_settings: EncoderSettings,
}

#[derive(Properties, Debug, PartialEq)]
pub struct MeetingProps {
    #[prop_or_default]
    pub id: String,
    pub client: VideoCallClient,
    #[prop_or_default]
    pub share_screen: bool, // Default to false
    #[prop_or_default]
    pub mic_enabled: bool, // Default to false
    #[prop_or_default]
    pub video_enabled: bool, // Default to false
    // Callback to parent component with formatted settings string
    pub on_encoder_settings_update: Callback<String>,
}

impl Component for Host {
    type Message = Msg;
    type Properties = MeetingProps;

    fn create(ctx: &Context<Self>) -> Self {
        let client = ctx.props().client.clone();
        info!("Creating Host component. Detecting platform...");

        // --- Callbacks for Encoder Settings ---
        let camera_callback = ctx.link().callback(Msg::CameraEncoderSettingsUpdated);
        let microphone_callback = ctx.link().callback(Msg::MicrophoneEncoderSettingsUpdated);
        let screen_callback = ctx.link().callback(Msg::ScreenEncoderSettingsUpdated);

        // --- Conditional Camera Encoder Creation ---
        let camera: Box<dyn CameraEncoderTrait> = if is_ios() {
            info!("Platform detected as iOS. Using IosCameraEncoder.");
            // Ensure IosCameraEncoder::new signature matches required parameters
            // Assuming new signature is new(client, element_id, bitrate_kbps, settings_callback)
            match IosCameraEncoder::new(
                client.clone(),
                VIDEO_ELEMENT_ID,
                VIDEO_BITRATE_KBPS, // Pass bitrate constant
                camera_callback,    // Pass settings callback
            ) {
                Ok(encoder) => Box::new(encoder),
                Err(e) => {
                    error!("Failed to create IosCameraEncoder: {:?}", e);
                    // Fallback to a dummy/non-functional encoder to avoid panic
                    // TODO: Implement a proper dummy encoder or handle error gracefully
                    Box::new(DummyCameraEncoder::new())
                }
            }
        } else {
            info!("Platform detected as non-iOS. Using standard CameraEncoder.");
            // Ensure CameraEncoder::new signature matches required parameters
            match CameraEncoder::new(
                client.clone(),
                VIDEO_ELEMENT_ID,
                VIDEO_BITRATE_KBPS, // Pass bitrate constant
                camera_callback,    // Pass settings callback
            ) {
                 Ok(encoder) => Box::new(encoder),
                 Err(e) => {
                    error!("Failed to create CameraEncoder: {:?}", e);
                    // Fallback to a dummy/non-functional encoder
                     Box::new(DummyCameraEncoder::new())
                 }
            }
        };

        // --- Standard Microphone and Screen Encoder Creation ---
        // TODO: Potentially apply conditional logic to microphone if AudioEncoder issues found on iOS
        let mut microphone = MicrophoneEncoder::new(
            client.clone(),
            AUDIO_BITRATE_KBPS, // Pass bitrate constant
            microphone_callback,
        )
        .expect("Failed to create MicrophoneEncoder"); // Handle potential errors

        let mut screen = ScreenEncoder::new(
            client.clone(),
            SCREEN_BITRATE_KBPS, // Pass bitrate constant
            screen_callback,
        )
        .expect("Failed to create ScreenEncoder"); // Handle potential errors

        // --- Setup Encoder Control Channels ---
        // Setup requires mutable access, do it after initial creation

        // Camera Control Channel
        { // Scope to contain mutable borrow of camera
            let mut camera_mut = camera; // Rebind as mutable, Note: Box is implicitly DerefMut if needed inside scope
            let (tx_cam, rx_cam) = mpsc::unbounded::<EncoderControlMessage>();
            // Assuming subscribe_diagnostics is infallible or returns Result
            client
                .subscribe_diagnostics(tx_cam, MediaType::VIDEO)
                .expect("Failed to subscribe video diagnostics");
            camera_mut.set_encoder_control(rx_cam); // Call method on the trait object
             // Explicitly drop mutable borrow before camera is moved into Self struct
             // Although moving it should also drop the temporary mutable borrow implicitly.
            drop(camera_mut);
        }


        // Microphone Control Channel
        let (tx_mic, rx_mic) = mpsc::unbounded::<EncoderControlMessage>();
        client
            .subscribe_diagnostics(tx_mic, MediaType::AUDIO)
            .expect("Failed to subscribe audio diagnostics");
        microphone.set_encoder_control(rx_mic); // Assuming method exists

        // Screen Control Channel
        let (tx_screen, rx_screen) = mpsc::unbounded::<EncoderControlMessage>();
        client
            .subscribe_diagnostics(tx_screen, MediaType::SCREEN)
            .expect("Failed to subscribe screen diagnostics");
        screen.set_encoder_control(rx_screen); // Assuming method exists


        info!("Host component created and encoders initialized.");

        Self {
            camera, // Store the Box<dyn Trait>
            microphone,
            screen,
            share_screen: ctx.props().share_screen,
            mic_enabled: ctx.props().mic_enabled,
            video_enabled: ctx.props().video_enabled,
            encoder_settings: EncoderSettings::default(), // Initialize empty
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        // --- Screen Share Logic ---
        let target_screen_enabled = ctx.props().share_screen;
        // Use set_enabled result *and* compare state to avoid redundant messages
        if self.screen.set_enabled(target_screen_enabled) || self.share_screen != target_screen_enabled {
            self.share_screen = target_screen_enabled; // Update internal state
            if target_screen_enabled {
                 // Delay before starting screen share to allow user prompt etc.
                 let link = ctx.link().clone();
                 let timeout = Timeout::new(1000, move || {
                     link.send_message(Msg::EnableScreenShare);
                 });
                 timeout.forget();
            } else {
                 ctx.link().send_message(Msg::DisableScreenShare);
            }
        }

        // --- Microphone Logic ---
        let target_mic_enabled = ctx.props().mic_enabled;
         if self.microphone.set_enabled(target_mic_enabled) || self.mic_enabled != target_mic_enabled {
            self.mic_enabled = target_mic_enabled; // Update internal state
            // EnableMicrophone msg handles start, DisableMicrophone handles stop
             ctx.link().send_message(if target_mic_enabled {
                 Msg::EnableMicrophone(true)
             } else {
                 Msg::DisableMicrophone
             });
         }

        // --- Camera Logic (Uses Trait Object) ---
        let target_video_enabled = ctx.props().video_enabled;
         if self.camera.set_enabled(target_video_enabled) || self.video_enabled != target_video_enabled {
            self.video_enabled = target_video_enabled; // Update internal state
            // EnableVideo msg handles start, DisableVideo handles stop
             ctx.link().send_message(if target_video_enabled {
                 Msg::EnableVideo(true)
             } else {
                 Msg::DisableVideo
             });
         }

        // --- Update Client State ---
        // TODO: Confirm if these methods exist on VideoCallClient and if they are necessary
        // ctx.props().client.set_audio_enabled(self.mic_enabled);
        // ctx.props().client.set_video_enabled(self.video_enabled);
        // ctx.props().client.set_screen_enabled(self.share_screen);

        if first_render {
            info!("Host component: First render, sending Start message.");
            ctx.link().send_message(Msg::Start);
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        debug!("Host update message received: {:?}", msg);
        let mut should_render = false; // Default to false unless state changes require UI update

        match msg {
            Msg::EnableScreenShare => {
                info!("Starting screen share encoder...");
                self.screen.start();
                // No state change directly affects this component's view, maybe settings update later
            }
            Msg::DisableScreenShare => {
                info!("Stopping screen share encoder...");
                self.screen.stop();
                if self.encoder_settings.screen.take().is_some() { // Clear settings if stopping
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    should_render = true; // Settings struct changed
                }
            }
            Msg::Start => {
                info!("Host component started.");
                // Initial setup actions could go here if needed beyond `rendered`
            }
            Msg::EnableMicrophone(should_enable) => {
                if should_enable && self.mic_enabled { // Ensure it's actually enabled
                    info!("Starting microphone encoder...");
                    self.microphone.start();
                } else if !should_enable {
                     // This case might be handled by DisableMicrophone message now
                     info!("EnableMicrophone(false) called but handled by DisableMicrophone path.");
                }
            }
            Msg::DisableMicrophone => {
                info!("Stopping microphone encoder...");
                self.microphone.stop();
                if self.encoder_settings.microphone.take().is_some() { // Clear settings
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    should_render = true;
                }
            }
            Msg::EnableVideo(should_enable) => {
                if should_enable && self.video_enabled { // Ensure it's actually enabled
                    info!("Starting camera encoder...");
                    self.camera.start(); // Call method on trait object
                } else if !should_enable {
                     info!("EnableVideo(false) called but handled by DisableVideo path.");
                }
            }
            Msg::DisableVideo => {
                info!("Stopping camera encoder...");
                self.camera.stop(); // Call method on trait object
                if self.encoder_settings.camera.take().is_some() { // Clear settings
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    should_render = true;
                }
            }
            Msg::AudioDeviceChanged(audio_device_id) => {
                info!("Audio device selection changed to: {}", audio_device_id);
                // Pass the selected device ID to the microphone encoder
                if self.microphone.select(audio_device_id) && self.mic_enabled {
                    // If selection changed AND mic is currently enabled, restart it after delay
                    info!("Microphone selection changed, restarting encoder...");
                    let link = ctx.link().clone();
                    // Short delay to allow device switch
                    let timeout = Timeout::new(500, move || {
                        link.send_message(Msg::EnableMicrophone(true));
                    });
                    timeout.forget();
                }
                // No direct render change needed for device selection itself
            }
            Msg::VideoDeviceChanged(video_device_id) => {
                info!("Video device selection changed to: {}", video_device_id);
                // Pass the selected device ID to the camera encoder (trait object)
                if self.camera.select(video_device_id) && self.video_enabled {
                     // If selection changed AND video is currently enabled, restart it after delay
                     info!("Camera selection changed, restarting encoder...");
                     let link = ctx.link().clone();
                     // Short delay to allow device switch
                     let timeout = Timeout::new(500, move || {
                         link.send_message(Msg::EnableVideo(true));
                     });
                    timeout.forget();
                }
                 // No direct render change needed
            }
            Msg::CameraEncoderSettingsUpdated(settings) => {
                debug!("Received camera settings update: {}", settings);
                if self.encoder_settings.camera.as_ref() != Some(&settings) {
                    self.encoder_settings.camera = Some(settings);
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    should_render = true; // Re-render if settings display changes
                }
            }
            Msg::MicrophoneEncoderSettingsUpdated(settings) => {
                 debug!("Received microphone settings update: {}", settings);
                 if self.encoder_settings.microphone.as_ref() != Some(&settings) {
                    self.encoder_settings.microphone = Some(settings);
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    should_render = true;
                }
            }
            Msg::ScreenEncoderSettingsUpdated(settings) => {
                 debug!("Received screen settings update: {}", settings);
                 if self.encoder_settings.screen.as_ref() != Some(&settings) {
                    self.encoder_settings.screen = Some(settings);
                    ctx.props()
                        .on_encoder_settings_update
                        .emit(self.encoder_settings.to_string());
                    should_render = true;
                }
            }
        };
        should_render // Only return true if the view needs updating
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let mic_callback = ctx.link().callback(Msg::AudioDeviceChanged);
        let cam_callback = ctx.link().callback(Msg::VideoDeviceChanged);
        html! {
            <>
                // Local video preview element
                 <video class="self-camera" autoplay=true playsinline=true muted=true id={VIDEO_ELEMENT_ID}></video>
                 // Device selector component
                 <DeviceSelector on_microphone_select={mic_callback} on_camera_select={cam_callback}/>
                 // Optional: Display encoder settings for debugging
                 // <pre>{ self.encoder_settings.to_string() }</pre>
            </>
        }
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        info!("Destroying Host component and stopping encoders.");
        // Call stop on trait objects/concrete types
        self.camera.stop();
        self.microphone.stop();
        self.screen.stop();
    }
}


// --- Dummy Encoder Implementation (for fallback / trait example) ---
// TODO: Place this appropriately, perhaps in its own module or conditionally compiled
#[derive(Debug, Default)]
pub struct DummyCameraEncoder {}

impl DummyCameraEncoder {
    pub fn new() -> Self { Self {} }
}

// Make dummy implement the trait so Box<dyn> works even on error
impl CameraEncoderTrait for DummyCameraEncoder {
    fn set_enabled(&mut self, _value: bool) -> bool { false }
    fn select(&mut self, _device_id: String) -> bool { false }
    fn start(&mut self) { error!("DummyCameraEncoder cannot start - check for initialization errors."); }
    fn stop(&mut self) {}
    fn set_encoder_control(&mut self, _rx: UnboundedReceiver<EncoderControlMessage>) {
         warn!("DummyCameraEncoder received encoder control channel - ignoring.");
    }
}


// NOTE: You will need to define the `CameraEncoderTrait` in a place accessible
// by both `host.rs` and the actual encoder implementations (`CameraEncoder`, `IosCameraEncoder`)
// likely within the `videocall-client` crate itself or a shared types crate if appropriate.
// You also need to ensure `CameraEncoder`, `IosCameraEncoder`, `MicrophoneEncoder`,
// and `ScreenEncoder` all properly implement the `set_encoder_control` method to receive
// and handle messages from the `VideoCallClient` via the MPSC channel.
// The `EncoderControlMessage` enum also needs to be defined.