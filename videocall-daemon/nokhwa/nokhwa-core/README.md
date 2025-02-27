# Disclaimer
This is a fork of nokhwa, tailored to the videocall ecosystem needs.

# videocall-nokhwa-core
This crate contains core type definitions for `videocall-nokhwa`. This is seperated so other crates may use it as well.

Inside there are standard definitions (`Resolution`, `CameraInfo`, `CameraIndex`, `CameraFormat`, etc.), and 
there are decoders for NV12, YUY2/YUYV, MJPEG, GRAY, and RGB24, with a flexible trait based system for you to add your
own decoders. 