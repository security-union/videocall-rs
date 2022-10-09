use js_sys::Array;
use js_sys::Boolean;
use js_sys::JsString;
use js_sys::Reflect;
use reqwasm::http::Request;
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlVideoElement;
use yew::prelude::*;
#[macro_use]
extern crate lazy_static;
use gloo_console::log;
use gloo_utils::document;
use gloo_utils::window;
use wasm_bindgen::JsCast;
use web_sys::*;
use yew_router::prelude::*;

// This is read at compile time, please restart if you change this value.
const ACTIX_PORT: &str = std::env!("ACTIX_PORT");
const LOGIN_URL: &str = std::env!("LOGIN_URL");
static VIDEO_CODEC: &str = "vp09.00.10.08";
const VIDEO_HEIGHT: i32 = 720i32;
const VIDEO_WIDTH: i32 = 1280i32;

fn truthy(s: String) -> bool {
    ["true".to_string(), "1".to_string()].contains(&s.to_lowercase())
}

// We need a lazy static block because these vars need to call a
// few functions.
lazy_static! {
    static ref ENABLE_OAUTH: bool = false;//truthy(std::env!("ENABLE_OAUTH").to_string());
}

#[derive(Properties, Debug, PartialEq)]
struct MeetingProps {
    #[prop_or_default]
    pub id: String
}

#[derive(Clone, Routable, PartialEq)]
enum Route {
    #[at("/login")]
    Login,
    #[at("/:id")]
    Meeting {id: String},
    #[not_found]
    #[at("/404")]
    NotFound,
}

fn switch(routes: &Route) -> Html {
    match routes {
        Route::Login => html! { <Login/> },
        Route::Meeting { id } => html! {
            <Meeting id={id.clone()}/>
        },
        Route::NotFound => html! { <h1>{ "404" }</h1> },
    }
}

#[function_component(Login)]
fn login() -> Html {
    let login = Callback::from(|_: MouseEvent| {
        window().location().set_href(LOGIN_URL).ok();
    });
    html! {<>
        <input type="image" onclick={login.clone()} src="/assets/btn_google.png" />
    </>}
}

#[function_component(App)]
fn app_component() -> Html {
    log!("OAuth enabled: {}", *ENABLE_OAUTH);
    if *ENABLE_OAUTH {
        html! {
            <BrowserRouter>
            <Switch<Route> render={Switch::render(switch)} />
            </BrowserRouter>
        }
    } else {
        html! {
            <Meeting id={"234234".to_string()}/>
        }
    }
}

#[function_component(Meeting)]
fn meeting(props: &MeetingProps) -> Html {
    if *ENABLE_OAUTH {
        let document = document().unchecked_into::<HtmlDocument>();

        // If there's a cookie, assume that we are logged in, else redirect to login page.
        if let Ok(e) = document.cookie() {
            // TODO: Validate cookie
            if e.is_empty() {
                window().location().set_href("/login").ok();
            }
        } else {
            window().location().set_href("/login").ok();
        }
    }
    let actix_url: String = format!("http://localhost:{}", ACTIX_PORT);
    let endpoint = Box::new(format!(
        "{actix_url}/hello/{name}",
        actix_url = actix_url,
        name = "world",
    ));
    let id = props.id.clone();

    use_effect_with_deps( move |_| {
        wasm_bindgen_futures::spawn_local( async move {
            let navigator = window().navigator();
            let media_devices = navigator.media_devices().unwrap();
            let video_element = window()
                .document()
                .unwrap()
                .get_element_by_id("webcam")
                .unwrap()
                .unchecked_into::<HtmlVideoElement>();
            
            let mut constraints = MediaStreamConstraints::new();
            constraints.video(&Boolean::from(true));
            let devices_query = media_devices
            .get_user_media_with_constraints(&constraints).unwrap();
            let device = JsFuture::from(devices_query)
                .await
                .unwrap()
                .unchecked_into::<MediaStream>();
            video_element.set_src_object(Some(&device));
            let video_track = Box::new(
                device.get_video_tracks()
                .find(&mut |_: JsValue, _:u32, _:Array | true)
                .unchecked_into::<VideoTrack>()
            );

            let error_handler = Closure::wrap(Box::new(move |e:JsValue| {
                console::log_1(&JsString::from("on errror"));
                console::log_1(&e);
            }) as Box<dyn FnMut(JsValue)>);

            let output_handler = Closure::wrap(Box::new(move | chunk: JsValue| {
                // let video_chunk = chunk.unchecked_into::<EncodedVideoChunk>();
                // video_context.dispatch(
                //     EncodedVideoChunkWrapper { chunk: Some(video_chunk)}
                // );
            }) as Box<dyn FnMut(JsValue)>);
            let video_encoder_init = VideoEncoderInit::new(
                error_handler.as_ref().unchecked_ref(),
                output_handler.as_ref().unchecked_ref()
            );
            let video_encoder = VideoEncoder::new(&video_encoder_init).unwrap();
            let settings = &mut video_track
                .clone()
                .unchecked_into::<MediaStreamTrack>()
                .get_settings();
            settings.width(VIDEO_WIDTH);
            settings.height(VIDEO_HEIGHT);
            let video_encoder_config = VideoEncoderConfig::new(
                &VIDEO_CODEC,
                VIDEO_HEIGHT as u32,
                VIDEO_WIDTH as u32
            );
            video_encoder.configure(&video_encoder_config);
            let processor = MediaStreamTrackProcessor::new(
                &MediaStreamTrackProcessorInit::new(
                    &video_track.unchecked_into::<MediaStreamTrack>(),
                )
            ).unwrap();
            let reader = processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>();
            loop {
                let result = JsFuture::from(reader.read())
                    .await
                    .map_err(|e| {
                        console::log_1(&e);
                    });
                match result {
                    Ok(js_frame) => {
                        let video_frame = 
                        Reflect::get(&js_frame, &JsString::from("value"))
                        .unwrap()
                        .unchecked_into::<VideoFrame>();
                        video_encoder.encode(&video_frame);
                        video_frame.close();
                    }
                    Err(_e) => {
                        console::log_1(&JsString::from("error"));
                    }
                }
            }
        });
    || ()
    }, (),
    );
        
    html!(
        <div class="producer">
            <h3>{"You"}</h3>
            <video autoplay=true id="webcam"></video>
        </div>
    )
}
fn main() {
    yew::start_app::<App>();
}
