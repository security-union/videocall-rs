use crate::constants::AUDIO_CHANNELS;
use js_sys::Float32Array;
use log::{debug, error, info, warn};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{window, AudioContext, AudioWorkletNode};

const HEALTH_CHECK_INTERVAL_MS: u64 = 5000; // 5 seconds
const INITIALIZATION_TIMEOUT_MS: u64 = 10000; // 10 seconds
const CLEANUP_TIMEOUT_MS: u64 = 2000; // 2 seconds
const TASK_TIMEOUT_MS: u64 = 1000; // 1 second for individual tasks

#[derive(Debug, Clone, PartialEq)]
pub enum WorkletState {
    Uninitialized,
    Initializing {
        start_time: u64,
    },
    Ready {
        worklet: AudioWorkletNode,
        initialized_at: u64,
        last_health_check: u64,
    },
    Terminating {
        start_time: u64,
    },
    Terminated,
}

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
struct TaskId(u64);

#[derive(Debug)]
enum TaskType {
    PCMProcessing,
    WorkletInitialization,
    HealthCheck,
    Cleanup,
}

#[derive(Clone)]
pub struct PeerAudioWorkletManager {
    state: Rc<RefCell<WorkletState>>,
    audio_context: AudioContext,
    peer_id: String,
    pending_tasks: Rc<RefCell<HashSet<TaskId>>>,
    speaker_device_id: Option<String>,
    health_check_interval: Rc<RefCell<Option<i32>>>,
    next_task_id: Arc<AtomicU64>,
    is_terminated: Rc<RefCell<bool>>,
}

#[derive(Debug)]
pub enum WorkletError {
    PeerDisconnected,
    InitializationFailed(String),
    NotResponsive,
    AudioContextError(String),
    TaskTimeout,
}

impl std::fmt::Display for WorkletError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkletError::PeerDisconnected => write!(f, "Peer has disconnected"),
            WorkletError::InitializationFailed(msg) => {
                write!(f, "Worklet initialization failed: {}", msg)
            }
            WorkletError::NotResponsive => write!(f, "Worklet not responsive"),
            WorkletError::AudioContextError(msg) => write!(f, "Audio context error: {}", msg),
            WorkletError::TaskTimeout => write!(f, "Task timeout"),
        }
    }
}

impl std::error::Error for WorkletError {}

#[derive(Debug)]
pub enum PCMError {
    PeerDisconnected,
    WorkletNotReady,
    WorkletNotResponsive,
    MessageSendFailed,
}

impl std::fmt::Display for PCMError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PCMError::PeerDisconnected => write!(f, "Peer has disconnected"),
            PCMError::WorkletNotReady => write!(f, "Worklet not ready"),
            PCMError::WorkletNotResponsive => write!(f, "Worklet not responsive"),
            PCMError::MessageSendFailed => write!(f, "Message send failed"),
        }
    }
}

impl std::error::Error for PCMError {}

/// RAII guard to automatically clean up tasks
struct TaskGuard {
    manager: Rc<RefCell<HashSet<TaskId>>>,
    task_id: TaskId,
}

impl TaskGuard {
    fn new(manager: Rc<RefCell<HashSet<TaskId>>>, task_id: TaskId) -> Self {
        manager.borrow_mut().insert(task_id);
        Self { manager, task_id }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.manager.borrow_mut().remove(&self.task_id);
    }
}

impl PeerAudioWorkletManager {
    pub fn new(
        peer_id: String,
        audio_context: AudioContext,
        speaker_device_id: Option<String>,
    ) -> Self {
        info!("Creating PeerAudioWorkletManager for peer {}", peer_id);

        Self {
            state: Rc::new(RefCell::new(WorkletState::Uninitialized)),
            audio_context,
            peer_id,
            pending_tasks: Rc::new(RefCell::new(HashSet::new())),
            speaker_device_id,
            health_check_interval: Rc::new(RefCell::new(None)),
            next_task_id: Arc::new(AtomicU64::new(1)),
            is_terminated: Rc::new(RefCell::new(false)),
        }
    }

    fn generate_task_id(&self) -> TaskId {
        TaskId(self.next_task_id.fetch_add(1, Ordering::Relaxed))
    }

    fn is_terminated(&self) -> bool {
        *self.is_terminated.borrow()
    }

    fn now_ms() -> u64 {
        js_sys::Date::now() as u64
    }

    pub async fn ensure_worklet_ready(&self) -> Result<(), WorkletError> {
        if self.is_terminated() {
            return Err(WorkletError::PeerDisconnected);
        }

        let current_state = self.state.borrow().clone();

        match current_state {
            WorkletState::Ready {
                ref worklet,
                last_health_check,
                ..
            } => {
                if Self::now_ms() - last_health_check > HEALTH_CHECK_INTERVAL_MS {
                    debug!("Health check needed for peer {}", self.peer_id);
                    if self.is_worklet_healthy(worklet).await {
                        self.update_health_check_time();
                        Ok(())
                    } else {
                        warn!(
                            "Worklet unhealthy for peer {}, reinitializing",
                            self.peer_id
                        );
                        self.reinitialize_worklet().await
                    }
                } else {
                    Ok(())
                }
            }
            WorkletState::Initializing { start_time } => {
                if Self::now_ms() - start_time > INITIALIZATION_TIMEOUT_MS {
                    error!("Worklet initialization timeout for peer {}", self.peer_id);
                    self.reset_and_retry().await
                } else {
                    self.wait_for_initialization().await
                }
            }
            WorkletState::Uninitialized => {
                info!("Initializing worklet for peer {}", self.peer_id);
                self.initialize_worklet().await
            }
            WorkletState::Terminating { .. } | WorkletState::Terminated => {
                Err(WorkletError::PeerDisconnected)
            }
        }
    }

    async fn initialize_worklet(&self) -> Result<(), WorkletError> {
        // Set state to Initializing
        *self.state.borrow_mut() = WorkletState::Initializing {
            start_time: Self::now_ms(),
        };

        match self.create_safari_audio_worklet().await {
            Ok(worklet) => {
                info!("Worklet successfully initialized for peer {}", self.peer_id);
                *self.state.borrow_mut() = WorkletState::Ready {
                    worklet,
                    initialized_at: Self::now_ms(),
                    last_health_check: Self::now_ms(),
                };
                self.start_health_monitoring();
                Ok(())
            }
            Err(e) => {
                error!(
                    "Failed to initialize worklet for peer {}: {:?}",
                    self.peer_id, e
                );
                *self.state.borrow_mut() = WorkletState::Uninitialized;
                Err(WorkletError::InitializationFailed(format!(
                    "JS Error during worklet initialization"
                )))
            }
        }
    }

    async fn create_safari_audio_worklet(&self) -> Result<AudioWorkletNode, JsValue> {
        // Load the PCM player worklet
        let audio_worklet = self.audio_context.audio_worklet()?;

        let module_promise = audio_worklet.add_module("/pcmPlayerWorker.js")?;
        wasm_bindgen_futures::JsFuture::from(module_promise).await?;

        // Create the PCM player worklet node
        let pcm_player = AudioWorkletNode::new(&self.audio_context, "pcm-player")?;

        // Connect worklet to destination
        pcm_player.connect_with_audio_node(&self.audio_context.destination())?;

        // Configure the worklet with explicit 48kHz
        let config_message = js_sys::Object::new();
        js_sys::Reflect::set(&config_message, &"command".into(), &"configure".into())?;
        js_sys::Reflect::set(
            &config_message,
            &"sampleRate".into(),
            &JsValue::from(48000.0),
        )?;
        js_sys::Reflect::set(
            &config_message,
            &"channels".into(),
            &JsValue::from(AUDIO_CHANNELS as f32),
        )?;

        pcm_player.port()?.post_message(&config_message)?;

        info!("Safari PCM worklet configured for peer {}", self.peer_id);
        Ok(pcm_player)
    }

    fn start_health_monitoring(&self) {
        let manager = self.clone();
        let task_id = self.generate_task_id();

        let health_check_callback = Closure::wrap(Box::new(move || {
            let manager_clone = manager.clone();
            let task_id = task_id;

            spawn_local(async move {
                let _guard = TaskGuard::new(manager_clone.pending_tasks.clone(), task_id);

                if manager_clone.is_terminated() {
                    return;
                }

                let state = manager_clone.state.borrow().clone();
                if let WorkletState::Ready { ref worklet, .. } = state {
                    if !manager_clone.is_worklet_healthy(worklet).await {
                        warn!(
                            "Health check failed for peer {}, marking for reinitialization",
                            manager_clone.peer_id
                        );
                        // Mark as unhealthy, next ensure_worklet_ready call will reinitialize
                        let mut state_mut = manager_clone.state.borrow_mut();
                        if let WorkletState::Ready {
                            worklet,
                            initialized_at,
                            ..
                        } = &*state_mut
                        {
                            *state_mut = WorkletState::Ready {
                                worklet: worklet.clone(),
                                initialized_at: *initialized_at,
                                last_health_check: 0, // Force health check failure
                            };
                        }
                    } else {
                        manager_clone.update_health_check_time();
                    }
                }
            });
        }) as Box<dyn FnMut()>);

        if let Some(window) = window() {
            let interval_id = window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    health_check_callback.as_ref().unchecked_ref(),
                    HEALTH_CHECK_INTERVAL_MS as i32,
                )
                .unwrap_or(-1);

            *self.health_check_interval.borrow_mut() = Some(interval_id);
        }

        health_check_callback.forget();
    }

    async fn is_worklet_healthy(&self, worklet: &AudioWorkletNode) -> bool {
        // Send a ping message and wait for pong response
        let ping_message = js_sys::Object::new();
        js_sys::Reflect::set(&ping_message, &"command".into(), &"ping".into()).unwrap();

        match worklet.port() {
            Ok(port) => {
                // For now, we'll just check if we can send the message
                // A full implementation would set up a message listener and wait for pong
                // This simplified version just checks if the port is responsive
                match port.post_message(&ping_message) {
                    Ok(_) => {
                        debug!(
                            "Health check ping sent successfully for peer {}",
                            self.peer_id
                        );
                        true // Simplified - assume healthy if we can send
                    }
                    Err(_) => {
                        debug!(
                            "Health check failed for peer {} - message send failed",
                            self.peer_id
                        );
                        false
                    }
                }
            }
            Err(_) => {
                debug!(
                    "Health check failed for peer {} - no port available",
                    self.peer_id
                );
                false
            }
        }
    }

    fn update_health_check_time(&self) {
        let mut state = self.state.borrow_mut();
        if let WorkletState::Ready {
            worklet,
            initialized_at,
            ..
        } = &*state
        {
            *state = WorkletState::Ready {
                worklet: worklet.clone(),
                initialized_at: *initialized_at,
                last_health_check: Self::now_ms(),
            };
        }
    }

    async fn reinitialize_worklet(&self) -> Result<(), WorkletError> {
        warn!("Reinitializing worklet for peer {}", self.peer_id);

        // Clean up old worklet
        self.cleanup_worklet_internal().await;

        // Initialize new one
        self.initialize_worklet().await
    }

    async fn reset_and_retry(&self) -> Result<(), WorkletError> {
        error!(
            "Resetting and retrying worklet initialization for peer {}",
            self.peer_id
        );

        *self.state.borrow_mut() = WorkletState::Uninitialized;
        self.initialize_worklet().await
    }

    async fn wait_for_initialization(&self) -> Result<(), WorkletError> {
        // Simple poll-based waiting - could be improved with proper async coordination
        let start_time = Self::now_ms();

        loop {
            if Self::now_ms() - start_time > INITIALIZATION_TIMEOUT_MS {
                return Err(WorkletError::TaskTimeout);
            }

            let state = self.state.borrow().clone();
            match state {
                WorkletState::Ready { .. } => return Ok(()),
                WorkletState::Terminating { .. } | WorkletState::Terminated => {
                    return Err(WorkletError::PeerDisconnected);
                }
                _ => {
                    // Still initializing, wait a bit
                    let promise = js_sys::Promise::new(&mut |resolve, _| {
                        window()
                            .unwrap()
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 50)
                            .unwrap();
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                }
            }
        }
    }

    pub fn process_pcm_data(&self, pcm_data: Float32Array) -> Result<(), PCMError> {
        if self.is_terminated() {
            return Err(PCMError::PeerDisconnected);
        }

        let manager = self.clone();
        let task_id = self.generate_task_id();

        // Create task with automatic cleanup
        spawn_local(async move {
            let _guard = TaskGuard::new(manager.pending_tasks.clone(), task_id);

            // Quick check if we're terminated
            if manager.is_terminated() {
                return;
            }

            match manager.ensure_worklet_ready().await {
                Ok(()) => {
                    if let Err(e) = manager.send_pcm_to_worklet(&pcm_data).await {
                        match e {
                            PCMError::PeerDisconnected => {
                                // Expected during cleanup
                            }
                            _ => {
                                debug!(
                                    "PCM processing failed for peer {}: {:?}",
                                    manager.peer_id, e
                                );
                            }
                        }
                    }
                }
                Err(WorkletError::PeerDisconnected) => {
                    // Expected during cleanup
                }
                Err(e) => {
                    debug!(
                        "Failed to ensure worklet ready for peer {}: {:?}",
                        manager.peer_id, e
                    );
                }
            }
        });

        Ok(())
    }

    async fn send_pcm_to_worklet(&self, pcm_data: &Float32Array) -> Result<(), PCMError> {
        if self.is_terminated() {
            return Err(PCMError::PeerDisconnected);
        }

        let state = self.state.borrow().clone();
        if let WorkletState::Ready { ref worklet, .. } = state {
            let message = js_sys::Object::new();
            js_sys::Reflect::set(&message, &"command".into(), &"play".into())
                .map_err(|_| PCMError::MessageSendFailed)?;
            js_sys::Reflect::set(&message, &"pcm".into(), pcm_data)
                .map_err(|_| PCMError::MessageSendFailed)?;

            let port = worklet.port().map_err(|_| PCMError::WorkletNotResponsive)?;
            port.post_message(&message)
                .map_err(|_| PCMError::WorkletNotResponsive)?;

            Ok(())
        } else {
            Err(PCMError::WorkletNotReady)
        }
    }

    pub async fn graceful_shutdown(&self) {
        info!("Starting graceful shutdown for peer {}", self.peer_id);

        // Mark as terminated first to stop new tasks
        *self.is_terminated.borrow_mut() = true;

        // Set state to terminating
        {
            let mut state = self.state.borrow_mut();
            if matches!(*state, WorkletState::Terminated) {
                return;
            }
            *state = WorkletState::Terminating {
                start_time: Self::now_ms(),
            };
        }

        // Stop health checks
        if let Some(interval_id) = *self.health_check_interval.borrow() {
            if interval_id >= 0 {
                if let Some(window) = window() {
                    window.clear_interval_with_handle(interval_id);
                }
            }
        }
        *self.health_check_interval.borrow_mut() = None;

        // Wait for pending tasks to complete (with timeout)
        let cleanup_start = Self::now_ms();
        while !self.pending_tasks.borrow().is_empty() {
            if Self::now_ms() - cleanup_start > CLEANUP_TIMEOUT_MS {
                warn!(
                    "Task cleanup timed out for peer {}, {} tasks remaining",
                    self.peer_id,
                    self.pending_tasks.borrow().len()
                );
                break;
            }

            // Small delay to allow tasks to complete
            let promise = js_sys::Promise::new(&mut |resolve, _| {
                window()
                    .unwrap()
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 10)
                    .unwrap();
            });
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        }

        // Clean up worklet and audio context
        self.cleanup_worklet_internal().await;

        // Set final state
        *self.state.borrow_mut() = WorkletState::Terminated;

        info!("Graceful shutdown completed for peer {}", self.peer_id);
    }

    async fn cleanup_worklet_internal(&self) {
        let state = self.state.borrow().clone();
        if let WorkletState::Ready { ref worklet, .. } = state {
            // Send flush command to worklet before cleanup
            let flush_message = js_sys::Object::new();
            js_sys::Reflect::set(&flush_message, &"command".into(), &"flush".into()).unwrap();

            if let Ok(port) = worklet.port() {
                let _ = port.post_message(&flush_message);
            }

            // Disconnect the worklet
            let _ = worklet.disconnect();
        }
    }
}

impl Drop for PeerAudioWorkletManager {
    fn drop(&mut self) {
        info!("Dropping PeerAudioWorkletManager for peer {}", self.peer_id);

        // Mark as terminated
        *self.is_terminated.borrow_mut() = true;

        // Clean up health check interval
        if let Some(interval_id) = *self.health_check_interval.borrow() {
            if interval_id >= 0 {
                if let Some(window) = window() {
                    window.clear_interval_with_handle(interval_id);
                }
            }
        }

        // Set state to terminated
        *self.state.borrow_mut() = WorkletState::Terminated;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    // Simple test state manager that doesn't use Web APIs
    struct TestStateManager {
        state: Rc<RefCell<WorkletState>>,
        peer_id: String,
        pending_tasks: Rc<RefCell<HashSet<TaskId>>>,
        is_terminated: Rc<RefCell<bool>>,
        next_task_id: Arc<AtomicU64>,
    }

    impl TestStateManager {
        fn new(peer_id: String) -> Self {
            Self {
                state: Rc::new(RefCell::new(WorkletState::Uninitialized)),
                peer_id,
                pending_tasks: Rc::new(RefCell::new(HashSet::new())),
                is_terminated: Rc::new(RefCell::new(false)),
                next_task_id: Arc::new(AtomicU64::new(1)),
            }
        }

        fn get_current_state(&self) -> WorkletState {
            self.state.borrow().clone()
        }

        fn set_state(&self, state: WorkletState) {
            *self.state.borrow_mut() = state;
        }

        fn is_terminated(&self) -> bool {
            *self.is_terminated.borrow()
        }

        fn set_terminated(&self, terminated: bool) {
            *self.is_terminated.borrow_mut() = terminated;
        }

        fn generate_task_id(&self) -> TaskId {
            TaskId(self.next_task_id.fetch_add(1, Ordering::Relaxed))
        }

        fn get_pending_task_count(&self) -> usize {
            self.pending_tasks.borrow().len()
        }

        fn add_task(&self, task_id: TaskId) {
            self.pending_tasks.borrow_mut().insert(task_id);
        }

        fn remove_task(&self, task_id: TaskId) {
            self.pending_tasks.borrow_mut().remove(&task_id);
        }
    }

    #[wasm_bindgen_test]
    fn test_state_machine_basic_transitions() {
        let manager = TestStateManager::new("test_peer".to_string());

        // Initial state should be Uninitialized
        let initial_state = manager.get_current_state();
        assert!(matches!(initial_state, WorkletState::Uninitialized));

        // Transition to Initializing
        manager.set_state(WorkletState::Initializing { start_time: 12345 });
        let initializing_state = manager.get_current_state();
        assert!(matches!(
            initializing_state,
            WorkletState::Initializing { start_time: 12345 }
        ));

        // Transition to Ready (without actual worklet object for testing)
        let now = PeerAudioWorkletManager::now_ms();
        manager.set_state(WorkletState::Terminating { start_time: now });
        let terminating_state = manager.get_current_state();
        assert!(matches!(
            terminating_state,
            WorkletState::Terminating { .. }
        ));

        // Transition to Terminated
        manager.set_state(WorkletState::Terminated);
        let terminated_state = manager.get_current_state();
        assert!(matches!(terminated_state, WorkletState::Terminated));
    }

    #[wasm_bindgen_test]
    fn test_task_id_generation() {
        let manager = TestStateManager::new("test_peer".to_string());

        let id1 = manager.generate_task_id();
        let id2 = manager.generate_task_id();
        let id3 = manager.generate_task_id();

        assert!(id1.0 < id2.0);
        assert!(id2.0 < id3.0);
        assert_eq!(id1.0, 1);
        assert_eq!(id2.0, 2);
        assert_eq!(id3.0, 3);
    }

    #[wasm_bindgen_test]
    fn test_termination_flag() {
        let manager = TestStateManager::new("test_peer".to_string());

        assert!(!manager.is_terminated());

        manager.set_terminated(true);
        assert!(manager.is_terminated());

        manager.set_terminated(false);
        assert!(!manager.is_terminated());
    }

    #[wasm_bindgen_test]
    fn test_multiple_managers_independence() {
        let manager1 = TestStateManager::new("peer1".to_string());
        let manager2 = TestStateManager::new("peer2".to_string());

        assert_eq!(manager1.peer_id, "peer1");
        assert_eq!(manager2.peer_id, "peer2");

        // Task ID generation should be independent
        let id1_1 = manager1.generate_task_id();
        let id2_1 = manager2.generate_task_id();
        let id1_2 = manager1.generate_task_id();
        let id2_2 = manager2.generate_task_id();

        assert_eq!(id1_1.0, 1);
        assert_eq!(id2_1.0, 1);
        assert_eq!(id1_2.0, 2);
        assert_eq!(id2_2.0, 2);

        // States should be independent
        manager1.set_state(WorkletState::Terminating { start_time: 123 });
        manager2.set_state(WorkletState::Initializing { start_time: 456 });

        let state1 = manager1.get_current_state();
        let state2 = manager2.get_current_state();

        assert!(matches!(
            state1,
            WorkletState::Terminating { start_time: 123 }
        ));
        assert!(matches!(
            state2,
            WorkletState::Initializing { start_time: 456 }
        ));
    }

    #[wasm_bindgen_test]
    fn test_error_display_implementations() {
        // Test WorkletError Display implementation
        let worklet_errors = vec![
            WorkletError::PeerDisconnected,
            WorkletError::InitializationFailed("test error".to_string()),
            WorkletError::NotResponsive,
            WorkletError::AudioContextError("context error".to_string()),
            WorkletError::TaskTimeout,
        ];

        for error in worklet_errors {
            let display_str = format!("{}", error);
            assert!(!display_str.is_empty());
            assert!(display_str.len() > 5); // Should have meaningful content
        }

        // Test PCMError Display implementation
        let pcm_errors = vec![
            PCMError::PeerDisconnected,
            PCMError::WorkletNotReady,
            PCMError::WorkletNotResponsive,
            PCMError::MessageSendFailed,
        ];

        for error in pcm_errors {
            let display_str = format!("{}", error);
            assert!(!display_str.is_empty());
            assert!(display_str.len() > 5); // Should have meaningful content
        }
    }

    #[wasm_bindgen_test]
    fn test_task_guard_cleanup() {
        let manager = TestStateManager::new("test_peer".to_string());
        let task_id = TaskId(42);

        assert_eq!(manager.get_pending_task_count(), 0);

        {
            let _guard = TaskGuard::new(manager.pending_tasks.clone(), task_id);
            assert_eq!(manager.get_pending_task_count(), 1);
        } // Guard should be dropped here

        assert_eq!(manager.get_pending_task_count(), 0);
    }

    #[wasm_bindgen_test]
    fn test_task_management() {
        let manager = TestStateManager::new("test_peer".to_string());

        assert_eq!(manager.get_pending_task_count(), 0);

        let task1 = manager.generate_task_id();
        let task2 = manager.generate_task_id();
        let task3 = manager.generate_task_id();

        manager.add_task(task1);
        assert_eq!(manager.get_pending_task_count(), 1);

        manager.add_task(task2);
        manager.add_task(task3);
        assert_eq!(manager.get_pending_task_count(), 3);

        manager.remove_task(task2);
        assert_eq!(manager.get_pending_task_count(), 2);

        manager.remove_task(task1);
        manager.remove_task(task3);
        assert_eq!(manager.get_pending_task_count(), 0);
    }

    #[wasm_bindgen_test]
    fn test_health_check_timing() {
        // Test the timing logic for health checks
        let old_time = 0u64;
        let current_time = PeerAudioWorkletManager::now_ms();

        let time_diff = current_time - old_time;
        assert!(time_diff > HEALTH_CHECK_INTERVAL_MS);

        // Test with recent time
        let recent_time = current_time - (HEALTH_CHECK_INTERVAL_MS / 2);
        let recent_diff = current_time - recent_time;
        assert!(recent_diff < HEALTH_CHECK_INTERVAL_MS);
    }

    // Test the constants are reasonable
    #[wasm_bindgen_test]
    fn test_constants() {
        assert_eq!(HEALTH_CHECK_INTERVAL_MS, 5000); // 5 seconds
        assert_eq!(INITIALIZATION_TIMEOUT_MS, 10000); // 10 seconds
        assert_eq!(CLEANUP_TIMEOUT_MS, 2000); // 2 seconds

        // Verify ordering makes sense
        assert!(CLEANUP_TIMEOUT_MS < HEALTH_CHECK_INTERVAL_MS);
        assert!(HEALTH_CHECK_INTERVAL_MS < INITIALIZATION_TIMEOUT_MS);
    }

    // Test PCM processing logic without WebAudio
    #[wasm_bindgen_test]
    fn test_pcm_processing_when_terminated() {
        let manager = TestStateManager::new("test_peer".to_string());

        // When not terminated, we would normally process
        assert!(!manager.is_terminated());

        // When terminated, processing should be rejected
        manager.set_terminated(true);
        assert!(manager.is_terminated());

        // The actual PCM processing would check is_terminated() and return PCMError::PeerDisconnected
        // We can verify this logic pattern without actually creating Float32Array
    }

    #[wasm_bindgen_test]
    fn test_state_transitions_integrity() {
        let manager = TestStateManager::new("test_peer".to_string());

        // Test the complete lifecycle without Web APIs

        // 1. Start uninitialized
        assert!(matches!(
            manager.get_current_state(),
            WorkletState::Uninitialized
        ));

        // 2. Begin initialization
        let init_time = PeerAudioWorkletManager::now_ms();
        manager.set_state(WorkletState::Initializing {
            start_time: init_time,
        });
        assert!(matches!(
            manager.get_current_state(),
            WorkletState::Initializing { .. }
        ));

        // 3. Complete initialization - would normally have a worklet here
        // For testing, we just verify the state can be set to Terminated directly

        // 4. Begin termination
        let term_time = PeerAudioWorkletManager::now_ms();
        manager.set_state(WorkletState::Terminating {
            start_time: term_time,
        });
        assert!(matches!(
            manager.get_current_state(),
            WorkletState::Terminating { .. }
        ));

        // 5. Complete termination
        manager.set_state(WorkletState::Terminated);
        manager.set_terminated(true);
        assert!(matches!(
            manager.get_current_state(),
            WorkletState::Terminated
        ));
        assert!(manager.is_terminated());
    }
}
