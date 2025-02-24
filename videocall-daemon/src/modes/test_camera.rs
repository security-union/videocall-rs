use nokhwa::{
    pixel_format::BgraFormat,
    utils::{CameraIndex, RequestedFormat, RequestedFormatType},
    Camera,
};
use pixels::{Pixels, SurfaceTexture};
use videocall_daemon::cli_args::{IndexKind, TestCamera};
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::{dpi::LogicalSize, event::VirtualKeyCode, window::WindowBuilder};

pub fn test_camera(info: TestCamera) {
    let video_device_index = match &info.video_device_index {
        IndexKind::String(s) => CameraIndex::String(s.clone()),
        IndexKind::Index(i) => CameraIndex::Index(*i),
    };
    let mut camera = Camera::new(
        video_device_index,
        RequestedFormat::new::<BgraFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
    )
    .unwrap();
    let actual_format = camera.camera_format();
    println!("Actual format: {:?}", actual_format);
    camera.open_stream().unwrap();
    // Image dimensions
    let width = actual_format.resolution().width();
    let height = actual_format.resolution().height();
    // Create window and event loop
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("BGRA Camera Feed")
        .with_inner_size(LogicalSize::new(width as f64, height as f64))
        .build(&event_loop)
        .unwrap();

    let window_scale = 1f64;
    let scaled_width = (width as f64 * window_scale) as u32;
    let scaled_height = (height as f64 * window_scale) as u32;
    let surface_texture = SurfaceTexture::new(width, height, &window);
    let mut pixels: Pixels = Pixels::new(scaled_width, scaled_height, surface_texture).unwrap();
    // print render format
    println!("Render format: {:?}", pixels.render_texture_format());
    println!("Texture format: {:?}", pixels.surface_texture_format());

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::RedrawRequested(_) => {
                // On the first frame, we need to resize the window to match the camera resolution

                // Grab a frame from the camera
                let camera_buffer = camera.frame().unwrap();
                // write to pixels
                // Transform image from NV12 to BGRA
                let pixels_frame = pixels.get_frame().len();
                let camera_buffer = camera_buffer.decode_image::<BgraFormat>().unwrap();
                let camera_buffer = camera_buffer.to_vec();
                let camera_frame = camera_buffer.len();
                // Write the frame to the pixels buffer, considering the format
                if camera_frame == pixels_frame {
                    pixels.get_frame_mut().copy_from_slice(&camera_buffer);
                } else {
                    // only copy enough to fill the pixels buffer,
                    pixels
                        .get_frame_mut()
                        .copy_from_slice(&camera_buffer[..pixels_frame]);

                    eprintln!(
                        "Frame sizes do not match: camera_frame: {}, pixels_frame: {}",
                        camera_frame, pixels_frame
                    );
                }

                // Render the frame
                if pixels.render().is_err() {
                    eprintln!("Pixels rendering failed!");
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::WindowEvent {
                event: WindowEvent::KeyboardInput { input, .. },
                ..
            } => {
                if let Some(VirtualKeyCode::Escape) = input.virtual_keycode {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {
                window.request_redraw();
            }
        }
    });
}
