
### Bug fix for username rename

When a user changes their display name during an active meeting, the app forces a full page reload, causing the user to loose their connection temporarily. This is because the app does not handle the username change event properly. The change causes yew  navigation to remount the page also causing the disconnection.





### My Solution - 1

The solution is to handle the username change event properly and update the user's display name in the app without causing a full page reload.
1. We need to introduce a stable session id(uuid) that is generated once on join and used for all communication( WebSocket, WebTransport). I think currently, the username/email is used as the session id. So when its changed, videocall-rs views it as a new user and causes a full page reload.

2. Changing the URL triggers Yew router navigation component remount full reconnection. The email parameter serves dual purposes: user identification AND display name

3. Also we can check MediaPacket carries the email, so the new change needs to affect the MediaPacket also.


### My Solution - 2
We can add the user username to the Websocket/WebTransport protocol message type for name update. This will allow local state not to disconnect and also broadcast the name update to all other participants. The client will send **NameUpdate** to the server, the server validates and broadcasts to all participants. Client update their local peer connection list.
