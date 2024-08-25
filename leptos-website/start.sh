#!/bin/bash

# Kill processes listening on ports 3000 and 3001
fuser -k 3000/tcp
fuser -k 3001/tcp

# # start tailwind
# npx tailwindcss -i ./input.css -o ./style/output.css --watch > /dev/null 2>&1 &

# Start Leptos
cargo leptos watch 
