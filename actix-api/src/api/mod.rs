/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

pub mod meetings;

use actix_web::web;

pub fn configure_api_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1")
            // List meetings and create meeting
            .service(
                web::resource("/meetings")
                    .route(web::get().to(meetings::list_meetings))
                    .route(web::post().to(meetings::create_meeting)),
            )
            // Get meeting info or delete meeting
            .service(
                web::resource("/meetings/{meeting_id}")
                    .route(web::get().to(meetings::get_meeting))
                    .route(web::delete().to(meetings::delete_meeting)),
            )
            // Join meeting (enter wait room)
            .service(
                web::resource("/meetings/{meeting_id}/join")
                    .route(web::post().to(meetings::join_meeting)),
            )
            // Get waiting room participants (host only)
            .service(
                web::resource("/meetings/{meeting_id}/waiting")
                    .route(web::get().to(meetings::get_waiting_room)),
            )
            // Admit participant (host only)
            .service(
                web::resource("/meetings/{meeting_id}/admit")
                    .route(web::post().to(meetings::admit_participant)),
            )
            // Admit all waiting participants (host only)
            .service(
                web::resource("/meetings/{meeting_id}/admit-all")
                    .route(web::post().to(meetings::admit_all_participants)),
            )
            // Reject participant (host only)
            .service(
                web::resource("/meetings/{meeting_id}/reject")
                    .route(web::post().to(meetings::reject_participant)),
            )
            // Get my status in meeting
            .service(
                web::resource("/meetings/{meeting_id}/status")
                    .route(web::get().to(meetings::get_my_status)),
            )
            // Leave meeting
            .service(
                web::resource("/meetings/{meeting_id}/leave")
                    .route(web::post().to(meetings::leave_meeting)),
            )
            // Get all admitted participants
            .service(
                web::resource("/meetings/{meeting_id}/participants")
                    .route(web::get().to(meetings::get_participants)),
            ),
    );
}
