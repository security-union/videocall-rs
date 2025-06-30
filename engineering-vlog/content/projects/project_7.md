+++
title = "Calcarine"
description = "A desktop app where live GPU shaders meet real-time local vision AI."
weight = 7

[extra]
# You can also crop the image in the url by adjusting w=/h=
local_image = "images/calcarine.jpeg"

+++

<h2><span style="color:orange;"> Calcarine </span></h2>

Calcarine is a desktop application that bridges the gap between GPU programming and modern AI. It allows you to process any visual stream—images, videos, or your live webcam feed—with custom compute shaders while simultaneously getting real-time scene analysis from a powerful Vision Language Model (VLM).

### Tech Highlights
*   Real-time video and image processing with **GPU compute shaders** (via `wgpu`).
*   Live VLM analysis using **Microsoft's PHI-3.5 Vision** model.
*   **Shader hot-reloading** for live-coding visual effects.
*   Built entirely in **Rust** on the **Cuneus** framework with an **egui** interface.

### About the AI Model
The project uses a CPU-optimized, INT4-quantized version of **Microsoft's PHI-3.5 Vision**. While this runs on the CPU, it's highly efficient and performs smoothly for real-time analysis (tested on a MacBook Air M3, 16GB RAM).

The primary goal is to demonstrate the pipeline of easily combining your own compute shaders with VLM analysis. As the ONNX and WebGPU ecosystems mature, the model backend will definitely be updated to leverage full GPU acceleration.

### Quick Start
1.  **Install GStreamer:** Required for video and webcam support. Download from the [official GStreamer website](https://gstreamer.freedesktop.org/download/).
2.  **Download Calcarine:** Grab the latest release for your operating system.
3.  **Run:** The first launch will automatically download the AI models (~3.2 GB). Press 'H' to toggle the UI.

---

[**Download from GitHub Releases**](https://github.com/altunenes/calcarine/releases)
<br>
[**View Source on GitHub**](https://github.com/altunenes/calcarine)