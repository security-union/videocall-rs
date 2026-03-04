
# videocall-meeting-types

`videocall-meeting-types` is a Rust library that provides the shared API types for the [videocall.rs](https://videocall.rs) meeting backend. This crate is framework-agnostic and serves as the single source of truth for the meeting API contract.

## Main Repo

If you are new to videocall you should start at our repo [videocall](https://github.com/security-union/videocall-rs)

## Features

- **Request Types**: `CreateMeetingRequest`, `JoinMeetingRequest`, `AdmitRequest`, `ListMeetingsQuery`
- **Response Types**: `APIResponse<T>` envelope, `ParticipantStatusResponse`, `MeetingInfoResponse`, `ProfileResponse`, and more
- **Error Types**: `APIError` with structured error codes
- **JWT Claims**: `RoomAccessTokenClaims` for media server room access tokens
- **Type Safety**: Strongly-typed structures shared between server and client crates

## Usage

Most consumers should use [`videocall-meeting-client`](../videocall-meeting-client) instead of depending on this crate directly. The client crate re-exports these types and provides a typed REST client for the meeting API.

This crate is a direct dependency only if you are building the meeting-api server itself or writing a custom client implementation.

## About `videocall.rs`

The `videocall.rs` system is an open-source, real-time teleconferencing platform built with Rust, WebTransport, and HTTP/3, designed for high-performance and low-latency communication.

## License

This project is dual-licensed under [MIT](../LICENSE-MIT) or [Apache-2.0](../LICENSE-APACHE).
