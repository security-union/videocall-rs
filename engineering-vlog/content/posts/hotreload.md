+++
title = "How I Implemented Hot Reloading for WGSL Shaders in Rust"
date = "2025-03-15"
[taxonomies]
tags=["rust","gpu"]
+++

<span style="color:orange;">My solution</span>

When developing WGSL shaders for my Rust-based graphics engine, I needed a solution to avoid constantly restarting the application to see changes. I built a hot reload system that watches shader files and automatically recompiles them when modifications are detected. The core of this approach uses Rust's notify crate to monitor file system events, combined with a debouncing mechanism to prevent multiple reloads during rapid file saves. When a change is detected, the engine creates new shader modules with `core.device.create_shader_module()` and carefully rebuilds the render pipeline while maintaining the original bind group layouts.

Important struct: 
[Source Code](https://github.com/altunenes/cuneus/blob/b068041c7902df29d33c3100ea4b74a1a38164ff/src/hot.rs#L9-L231)

```rust
pub struct ShaderHotReload {
    pub vs_module: wgpu::ShaderModule,
    pub fs_module: wgpu::ShaderModule,
    device: Arc<wgpu::Device>,
    shader_paths: Vec<PathBuf>,
    last_vs_content: String,
    last_fs_content: String,
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
    rx: Receiver<notify::Event>,
    _watcher_tx: std::sync::mpsc::Sender<notify::Event>,
    last_update_times: HashMap<PathBuf, Instant>, //Keeps track of when each shader file was last updated.
    debounce_duration: Duration, //Defines how long to wait before allowing another reload of the same file. The default is 100ms.
}
```

My ShaderHotReload struct stores references to shader files, tracks the last update times for debouncing, and maintains the original shader content for comparison. When a file change is detected, it reads the new shader content, compares it to the previous version, and only triggers a reload if there's an actual change.