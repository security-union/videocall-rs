Bot
===

The bot connects to a specified websocket endpoint and sends hello messages every second. It utilizes Tokio runtime to support multiple concurrent websocket clients.

To build and run the application, execute the following commands:

```
N_CLIENTS=1 ENDPOINT=ws://localhost:3030 ROOM=redrum ECHO_USER=test cargo run
```

Before running the application, make sure to set the environment variables `N_CLIENTS`, `ENDPOINT`, `ROOM`, and `ECHO_USER`.

