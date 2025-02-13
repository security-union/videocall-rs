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

    camera.open_stream().unwrap();
    // Image dimensions
    let width = actual_format.resolution().width();
    let height = actual_format.resolution().height();
    println!("Image dimensions: {}x{}", width, height);

    // Create window and event loop
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Camera Feed")
        .with_inner_size(LogicalSize::new(width as f64, height as f64))
        .build(&event_loop)
        .unwrap();

    let surface_texture = SurfaceTexture::new(width, height, &window);
    let mut pixels: Pixels = Pixels::new(width, height, surface_texture).unwrap();
    // print render format
    println!("Render format: {:?}", pixels.render_texture_format());
    println!("Texture format: {:?}", pixels.surface_texture_format());
    println!("Camera format: {:?}", actual_format);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::RedrawRequested(_) => {
                let frame_mut = pixels.get_frame_mut();
                camera
                    .write_frame_to_buffer::<BgraFormat>(frame_mut)
                    .unwrap();

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
