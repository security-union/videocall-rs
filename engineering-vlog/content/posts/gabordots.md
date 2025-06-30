+++
title = "Illusory Movement of Dotted Lines but Gabor Version"
date = "2024-12-05"
[taxonomies]
tags=["gabor","shader","illusion","vision"]
+++


## <span style="color:orange;">Introduction</span>
Today, I was exploring [Michael Bach's](https://michaelbach.de/ot/mot-dottedLines/index.html) comments about some illusions and I stumbled upon an illusion called "Dotted Line Motion Illusion" that I hadn't known before. I wanted to read more about it because the effect didn't seem to work well for me. Both Bach and the original article used a rectangular checkerboard design for the illusion. I tried to reproduce the code in ShaderToy and noticed that the scale of the rectangles and the background color significantly affect the strength of the illusion, at least from my perception.

## <span style="color:orange;">Plain Version</span>
First, investigate the illusion below. Click the play button, track the red disc as it moves, and notice how the checkered lines seem to shift. It works best on a big screen.

<div align="center">
<iframe width="640" height="450" frameborder="0" src="https://www.shadertoy.com/embed/XcKXRV?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>
</div>

After finishing the coding, I pondered whether "The reason is that the black/white contrast signals between adjacent dots along the length of the line are stronger than black/grey or white/grey contrast signals across the line, and the motion is computed as a vector sum of local contrast-weighted motion signals." could be an explanation, and then, could Gabor patches be more effective here?

## <span style="color:orange;">Gabor Version</span>
Here's what I did. Now, try this and see which one appears stronger. Interestingly, even on a smaller screen, this version works much better for me and it's really functioning very nicely.

<div align="center">
<iframe width="640" height="450" frameborder="0" src="https://www.shadertoy.com/embed/McKSRK?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>
</div>

## <span style="color:orange;">Exploring Reverse Effects</span>
Even more interestingly, I discovered a reverse effect when I animated the phase offset. Follow the red dot again, and you'll notice that the phase movement stops at some point.

<div align="center">
<iframe width="640" height="450" frameborder="0" src="https://www.shadertoy.com/embed/4fyXz3?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>
</div>

## <span style="color:orange;">Reference Paper</span>
For more detailed information on the scientific background of these visual phenomena, refer to the following paper:

Ito, H., Anstis, S., & Cavanagh, P. (2009). Illusory Movement of Dotted Lines. Perception, 38(9), 1405-1409. [https://doi.org/10.1068/p6383](https://doi.org/10.1068/p6383)
