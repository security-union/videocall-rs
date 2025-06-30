+++
title = "Audio Visualizer"
description = "A GPU-accelerated audio visualizer for music built with my own graphic engine, Cuneus"
weight = 6

[extra]
# You can also crop the image in the url by adjusting w=/h=
local_image = "images/audiovis.png"
+++

my new audio visualization tool built with my graphics engine, Cuneus.
Spectrum and BPM analysis made in the CPU/Rust side, shader is WGSL. 

## <span style="color:orange;"> Video Preview </span>

<div style="display: flex; justify-content: center; margin: 2rem 0;">
<blockquote class="twitter-tweet" data-media-max-width="560"><p lang="en" dir="ltr">I created an audio vis within my wgsl shader tool with Rust 🦀<br>note: Spectrum analysis on the CPU side via Gstreamer<a href="https://t.co/XPeSaWMzPD">https://t.co/XPeSaWMzPD</a><a href="https://twitter.com/hashtag/rustlang?src=hash&amp;ref_src=twsrc%5Etfw">#rustlang</a> <a href="https://twitter.com/hashtag/dailycoding?src=hash&amp;ref_src=twsrc%5Etfw">#dailycoding</a> <a href="https://t.co/tJE1HFOmmM">pic.twitter.com/tJE1HFOmmM</a></p>&mdash; enes altun (@emportent) <a href="https://twitter.com/emportent/status/1900840774977143168?ref_src=twsrc%5Etfw">March 15, 2025</a></blockquote> <script async src="https://platform.twitter.com/widgets.js" charset="utf-8"></script>
</div>

## <span style="color:orange;"> Download </span>

You can download on here with different OS: (search for 'audiovis' in the github releases page)

[download](https://github.com/altunenes/cuneus/releases "audiovis")

## <span style="color:orange;"> Small Tech Details </span>


On the CPU side, GStreamer processes the audio stream to extract real-time spectrum data across 128 frequency bands with specialized handling for bass, mid, and high ranges. The system also incorporates BPM detection algorithms with octave correction to accurately identify musical tempo regardless of genre. But to be honest, its not stable. While coding this,BPM detection turned out to be much more challenging than I expected. spectrum analysis was much easier. The following article I found was very helpful in this process:

https://www.ifs.tuwien.ac.at/~knees/publications/hoerschlaeger_etal_smc_2015.pdf

The WGSL shader pipeline transforms this audio data into dynamic visualizations with frequency-responsive equalizer bars, reactive waveform displays, and color-cycling effects. All rendering is GPU-accelerated through WebGPU, means smooth performance even with complex visual effects while maintaining synchronized audio-visual correlation. :-) 
