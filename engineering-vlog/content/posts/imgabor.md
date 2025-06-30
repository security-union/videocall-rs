+++
title = "Gabor Patches on the Texture!"
date = "2023-10-16"
[taxonomies]
tags=["image","shader","gabor","face","vision"]
+++

Below is a video featuring the outcome of my experiment. Take a close look and tell me, do you perceive the face in the image more in the horizontal or vertical orientation of the Gabor patches?


<div align="center">

<blockquote class="twitter-tweet" data-media-max-width="560"><p lang="en" dir="ltr">with different orientations of Gabors... Actually interesting, because this reminds me of research underscoring the important role of horizontal info in face perception, revealing its crucial impact on facial encoding/processing while vertical info often reduces these effects. 🙂 <a href="https://t.co/mUcCwJ4SBc">https://t.co/mUcCwJ4SBc</a> <a href="https://t.co/ghXyMWS7CO">pic.twitter.com/ghXyMWS7CO</a></p>&mdash; enes altun (@emportent) <a href="https://twitter.com/emportent/status/1713689728195690576?ref_src=twsrc%5Etfw">October 15, 2023</a></blockquote> <script async src="https://platform.twitter.com/widgets.js" charset="utf-8"></script>

</div>


If you've been following my previous posts, you're already familiar with the concept of Gabor patches and how they're a fantastic tool for understanding various aspects of visual perception. What's particularly intriguing is how these patches can alter our perception of orientation.

findings of [Dakin and Watt (2009)](https://pubmed.ncbi.nlm.nih.gov/19757911/) and others(there are a lot of papers about that issue), our perception of faces is significantly influenced by horizontal information compared to vertical. Some call this as [radial bias](https://royalsocietypublishing.org/doi/10.1098/rspb.2023.1118#:~:text=The%20radial%20bias%20may%20modulate,the%20individual%20differences%20we%20observe.)

Building on this intriguing concept, Dakin and Watt's study delves even deeper into our visual system's preference for horizontal features in faces. They introduced the idea of facial 'bar codes,' unique clusters of horizontal lines, akin to commercial bar codes, that our brains use for quick and efficient face recognition. This theory elegantly explains why we're so adept at recognizing faces under various conditions and why certain transformations, like inverting a face, make recognition remarkably challenging. It's a compelling reminder of how our visual system has fine-tuned itself over millennia, optimizing certain perceptual shortcuts for survival.

In a nutshell, when the Gabor patches are aligned horizontally, we're likely to perceive the face more clearly. This phenomenon ties back to how our brains process faces, giving preferential treatment to horizontal features. It's all about how the human visual system has evolved to prioritize certain spatial frequencies and orientations, especially when it comes to recognizing faces - one of the most crucial visual tasks we perform.

But hey, don't just take my word for it! Dive into the research, play around with the code, and explore the captivating realm of Gabor patches and visual perception. Who knows what other secrets are waiting to be uncovered?


Note: This code is computationally intensive, so only run it if you're confident in your GPU's capabilities. Also, please be cautious when adjusting the numbers—small changes can have big impacts!"
<div align="center">

<iframe width="420" height="360" frameborder="0" src="https://www.shadertoy.com/embed/Dd3fRB?gui=true&t=10&paused=true&muted=false" allowfullscreen></iframe>

</div>
