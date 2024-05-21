## Live website

https://www.rustlemania.com/

## News 

https://www.reddit.com/r/rust/comments/14vwxfh/we_just_released_our_rust_webtransport/

## ▶️ YouTube Videos

Here's how we scaled it to support 1000 user per call
https://youtu.be/LWwOSZJwEJI

The initial POC from 2022
https://www.youtube.com/watch?v=kZ9isFw1TQ8

# video-call.rs

MVP of a teleconferencing system written in rust, both the backend and the UI.

# How to try it out?

## Setup 
Technically you could run this with a single computer, but it is more fun if you use 2+.

## Steps

1. Open chrome://flags on all the computers that you want to use to test the tele-conferencing system, add the ip of the computer that you will use as the server to the Insecure origins treated as secure list.
<img width="1728" alt="Screen Shot 2022-10-30 at 10 00 43 PM" src="https://user-images.githubusercontent.com/1176339/198916116-e85bd52a-02b3-40ed-9764-d08fd3df8487.png">

2. Start the servers on the computer that you intend to use as the server using `ACTIX_UI_BACKEND_URL=ws://<server-ip>:8080 make up` (requires docker).

3. If your server computer is behind a firewall, make sure that TCP ports 80 and 8080 are open

4. Connect all computers to `http://<server-ip>/meeting/<username>/<meeting-id>`

5. Make sure that you "allow" access to your mic and camera:
<img width="1840" alt="Screen Shot 2022-10-24 at 8 23 50 AM" src="https://user-images.githubusercontent.com/1176339/197536159-61f0d9c8-c8fa-454c-8f40-404ed52dca98.png">

6. Click connect on both browsers, and enjoy:

![Oct-24-2022 08-37-09](https://user-images.githubusercontent.com/1176339/197853024-171e0dcc-2098-4780-b3be-bfc3cb5adb43.gif)

## ▶️ YouTube Channel
https://www.youtube.com/@securityunion

## 👉 Join our Discord Community
You can join our Discord Community, here is the [invite link](https://discord.gg/JP38NRe4CJ).

## 🫵 We need your haaalp!
We take pride on being community driven, there are many ways to collaborate, from filing an issue or, if you want to dive deeper,
being part of our [RFC Process](/rfc)

## 👨‍💻 Project Structure

Contains 3 sub-projects

1. actix-api: actix web server
2. yew-ui: Yew frontend
3. types: json serializable structures used to communicate the frontend and backend.

# Local Development

## Dockerized workflow

1. Install docker https://docs.docker.com/engine/install/
2. Install buildx https://github.com/docker/buildx?tab=readme-ov-file#installing
3. Run one of the supported make commands
```
make test
make up
make down
make build
```

> Currently the Dockerfile in the repositories root is different from the files in `docker/`.
> This will be refactored shortly

## Without docker

### Prerequisites

1. Create a postgresql database that you can accessed without a password.  The default database name is `actix-api-db`, i.e. the default connection string is `postgresql://$USER@localhost/actix-api-db`

2. Install [trurl](see https://github.com/curl/trurl) and [nats-server]((see https://docs.nats.io/running-a-nats-service/introduction/installation)

### Starting up the servers

1. Run the script `./start_dev.sh`.

   It examines various environment variables to control the behavior; see the script itself for details.
   By default it runs using websockets rather than webtransport (`WEBTRANSPORT_ENABLED=0`) and without encryption (`E2EE_ENABLED=0`).

2. Connect your browser to `http://localhost:8081/meeting/<username>/<meeting-id>`

   You can make multiple connections (with varying usernames) from multiple browser windows or tabs.

   If you are using encryption (`E2EE_ENABLED=1`), you should lanuch Chrome with
   the necessary options for it to accept the local certificate by running `./launch_chrome.sh`

## 👤 Contributors ✨

<table>
<tr>
<td align="center"><a href="https://github.com/darioalessandro"><img src="https://avatars0.githubusercontent.com/u/1176339?s=400&v=4" width="100" alt=""/><br /><sub><b>Dario</b></sub></a></td>
<td align="center"><a href="https://github.com/griffobeid"><img src="https://avatars1.githubusercontent.com/u/12220672?s=400&u=639c5cafe1c504ee9c68ad3a5e09d1b2c186462c&v=4" width="100" alt=""/><br /><sub><b>Griffin Obeid</b></sub></a></td>    
<td align="center"><a href="https://github.com/ronen"><img src="https://avatars.githubusercontent.com/u/125620?v=4" width="100" alt=""/><br /><sub><b>Leone</b></sub></a></td>
<td align="center"><a href="https://github.com/leon3s"><img src="https://avatars.githubusercontent.com/u/7750950?v=4" width="100" alt=""/><br /><sub><b>Leone</b></sub></a></td>
<td align="center"><a href="https://github.com/JasterV"><img src="https://avatars3.githubusercontent.com/u/49537445?v=4" width="100" alt=""/><br /><sub><b>Victor Martínez</b></sub></a></td>
</tr>
</table>

The Actix websocket implementation contains fragments from https://github.com/JasterV/chat-rooms-actix in particular the usage of an actor to orchestrate all sessions and rooms.

## Show your support

Give a ⭐️ if this project helped you!
