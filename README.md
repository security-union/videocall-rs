# Rust Zoom Research Project

![zoom-rs](https://user-images.githubusercontent.com/1176339/197537597-2e9147dc-5892-47c9-9a7d-d5b0102800db.png)

MVP of a teleconferencing system written in rust, both the backend and the UI.

Security Union LLC is not associated with Zoom Video Communications, we are big fans of their products!!

## ▶️ YouTube Videos

- The initial POC from 2022 <https://www.youtube.com/watch?v=kZ9isFw1TQ8>
- Here's how we scaled it to support 1000 user per call <https://youtu.be/LWwOSZJwEJI>

## 🧪 How to test?

### 🖥️ Technical Setup

Technically you could test this with a single computer, but it is more fun if you use 2+ computers so that you can see how the video + audio streaming works better.

### 🐾 Steps

1. Open chrome://flags on all the computers that you want to use to test the tele-conferencing system, add the ip of the computer that you will use as the server to the Insecure origins treated as secure list.
<img width="1728" alt="Screen Shot 2022-10-30 at 10 00 43 PM" src="https://user-images.githubusercontent.com/1176339/198916116-e85bd52a-02b3-40ed-9764-d08fd3df8487.png">

2. Start the servers on the computer that you intend to use as the server using `ACTIX_UI_BACKEND_URL=ws://<server-ip>:8080 make up` (requires docker).

3. If your server computer is behind a firewall, make sure that TCP ports 80 and 8080 are open

4. Connect all computers to `http://<server-ip>/meeting/<username>/<meeting-id>`

5. Make sure that you "allow" access to your mic and camera:
<img width="1840" alt="Screen Shot 2022-10-24 at 8 23 50 AM" src="https://user-images.githubusercontent.com/1176339/197536159-61f0d9c8-c8fa-454c-8f40-404ed52dca98.png">

6. Click connect on both browsers, and enjoy:

![Oct-24-2022 08-37-09](https://user-images.githubusercontent.com/1176339/197853024-171e0dcc-2098-4780-b3be-bfc3cb5adb43.gif)

## 🌐 Community

### ▶️ YouTube Channel

<https://www.youtube.com/@securityunion>

### 👉 Join our Discord Community

You can join our Discord Community, here is the [invite link](https://discord.gg/JP38NRe4CJ).

## 👨‍💻 Project Structure

Contains 3 sub-projects

1. actix-api: actix web server
2. yew-ui: Yew frontend
3. types: json serializable structures used to communicate the frontend and backend.

## 🐳 Dockerized Workflow

1. Install docker (we recommend using [Orbstack](https://orbstack.dev/) over Docker Desktop for GUI support)
2. Run one of the supported make commands

```sh
# Production containers
make test
make up
make down
make build
# Development containers
make dev
```

### 🛠️ Dev Containers

This project uses [devcontainers](https://code.visualstudio.com/docs/remote/containers) to provide a consistent development environment.

## 👤 Contributors

<table>
<tr>
<td align="center"><a href="https://github.com/darioalessandro"><img src="https://github.com/darioalessandro.png?size=200" width="100" alt=""/><br /><sub><b>Dario</b></sub></a></td>
<td align="center"><a href="https://github.com/griffobeid"><img src="https://github.com/griffobeid.png?size=200" width="100" alt=""/><br /><sub><b>Griffin Obeid</b></sub></a></td>
<td align="center"><a href="https://github.com/JasterV"><img src="https://github.com/JasterV.png?size=200" width="100" alt=""/><br /><sub><b>Victor Martínez</b></sub></a></td>
<td align="center"><a href="https://github.com/leon3s"><img src="https://github.com/leon3s.png?size=200" width="100" alt=""/><br /><sub><b>Leone</b></sub></a></td>
<td align="center"><a href="https://github.com/ImBIOS"><img src="https://github.com/ImBIOS.png?size=200" width="100" alt=""/><br /><sub><b>ImBIOS</b></sub></a></td>
</tr>
</table>

The Actix websocket implementation contains fragments from <https://github.com/JasterV/chat-rooms-actix> in particular the usage of an actor to orchestrate all sessions and rooms.

## ✨ Show Your Support

Give a ⭐️ if this project helped you!

## 📝 Legal Notice

ZOOM is a trademark of Zoom Video Communications, Inc.

Security Union LLC is not associated with Zoom Video Communications, but we are big fans of their product!!

This project was created to learn about video + audio streaming using only RUST (with some html + css).

### 📝 License

[MIT](./LICENSE)
