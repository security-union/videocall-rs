+++
title = "Asahi illusion on the Fractal!"
date = "2023-09-30"
[taxonomies]
tags=["illusion","asahi","fractal","vision","shader","Rust"]
+++

## <span style="color:orange;"> Background </span>

While experimenting with shader code, I stumbled upon a fascinating visual phenomenon. When focusing on the center of a particular design, surrounded by colorful petals, the center appears brighter than it actually is. 

<div align="center">
<iframe width="640" height="360" frameborder="0" src="https://www.shadertoy.com/embed/DsfyRX?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>
</div>

Dr. Bruno Laeng's study unveiled that our brain is deceived into triggering a pupillary light reflex, causing our pupils to constrict as if protecting our eyes from intense light, like the sun's rays.


The Asahi illusion's effect on the pupil isn't instantaneous. In humans, there's a notable delay between the onset of the illusion and the pupillary response. This delay might be attributed to the time required for the brain's processing mechanisms to influence the pupillary light reflex.

Interestingly, the Visual Cortex (V1) seems to play a pivotal role. The V1 response to the Asahi illusion precedes the pupil constriction, suggesting its potential involvement in modulating the Autonomic Nervous System (ANS). However, the exact pathways, be it direct projections or intricate subcortical synapses, remain a topic of ongoing [research](https://academic.oup.com/cercor/article/33/12/7952/7084649?login=false) .

here is another one I coded after the above one.
<div align="center">

<iframe width="640" height="360" frameborder="0" src="https://www.shadertoy.com/embed/MX23Wz?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>
</div>

<div align="center">

<iframe width="640" height="360" frameborder="0" src="https://www.shadertoy.com/embed/43SGDh?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>

</div>

Note, you can download the asahi illusion demos with using interactive GUI I implemented (easy to change the parameters like colors etc) on here: (I compiled them with Rust :-) [Source code:](https://github.com/altunenes/rusty_art)

## <span style="color:orange;"> Asahi Demo Downloads </span>

| Software Version | Operating System | Download Link                                                                                     |
|------------------|------------------|----------------------------------------------------------------------------------------------------|
| **Asahi**        | macOS            | [Download](https://github.com/altunenes/rusty_art/releases/download/v1.0.4/asahi-macos-latest.zip) |
|                  | Ubuntu           | [Download](https://github.com/altunenes/rusty_art/releases/download/v1.0.4/asahi-ubuntu-latest.zip)|
|                  | Windows          | [Download](https://github.com/altunenes/rusty_art/releases/download/v1.0.4/asahi-windows-latest.zip)|
| **Asahi2**       | macOS            | [Download](https://github.com/altunenes/rusty_art/releases/download/v1.0.4/asahi2-macos-latest.zip) |
|                  | Ubuntu           | [Download](https://github.com/altunenes/rusty_art/releases/download/v1.0.4/asahi2-ubuntu-latest.zip)|
|                  | Windows          | [Download](https://github.com/altunenes/rusty_art/releases/download/v1.0.4/asahi2-windows-latest.zip)|






