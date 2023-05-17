Bot
===

The bot connects to a specified websocket endpoint and echoes messages produces by user ECHO_USER which is an env var passed to the bot command.

To build and run the application, execute the following commands:

```
N_CLIENTS=1 ENDPOINT=ws://localhost:3030 ROOM=redrum ECHO_USER=test cargo run
```

Before running the application, make sure to set the environment variables `N_CLIENTS`, `ENDPOINT`, `ROOM`, and `ECHO_USER`.

