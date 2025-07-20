#!/bin/sh

# Navigate to the Yew app directory
cd /app/yew-ui

# Recompile the Yew application with current environment variables
trunk build --release

# Copy the build results to the Nginx directory
cp -r dist/* /usr/share/nginx/html/

# Start Nginx in the foreground
exec nginx -g "daemon off;" 