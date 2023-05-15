Bot
===

The bot connects to a specified websocket endpoint and sends hello messages every second. It utilizes Tokio runtime to support multiple concurrent websocket clients.

To build and run the application, execute the following commands:

1. Build the application:

```
make build
```

2. Run the application:

```
make run
```

3. To test the application, run:

```
make test
```

Before running the application, make sure to set the environment variables `N_CLIENTS`, `ENDPOINT`, and `ROOM`.