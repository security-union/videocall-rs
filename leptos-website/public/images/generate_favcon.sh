ffmpeg -i icon.png -vf scale=16:16 favicon-16.png
ffmpeg -i icon.png -vf scale=32:32 favicon-32.png
ffmpeg -i icon.png -vf scale=48:48 favicon-48.png

ffmpeg -i favicon-16.png -i favicon-32.png -i favicon-48.png -c:v copy favicon.ico
