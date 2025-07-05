+++
title = "How to Concentrate and Code at 2 AM"
date = 2025-07-05
# Set to `true` while drafting; switch to `false` once published
draft = false
slug = "how-to-concentrate-and-code-at-2-am"
description = "TBD"
tags = ["extreme ownership", "staff engineer", "software reliability", "backend architecture", "observability", "rust", "autonomous vehicles", "leadership", "devops"]
authors = ["Dario Lencina Talarico"]

[extra]
seo_keywords = ["senior engineer", "software reliability", "backend architecture", "autonomous vehicles", "rust", "observability", "may mobility", "pagerduty"]

[taxonomies]
tags = ["extreme ownership", "staff engineer", "software reliability", "backend architecture", "observability", "rust", "autonomous vehicles", "leadership", "devops"]
authors = ["Dario Lencina Talarico"]
+++

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/4-am-coding.png" alt="4 AM coding session" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>


## The Why Behind Late-Night Coding

Research shows that people actually perform better on creative problem-solving tasks during their non-optimal times of day - when they're slightly tired and less inhibited<sup>[2]</sup>. This reduced frontal lobe activity allows for more creative connections between ideas, making those 2 AM coding sessions surprisingly effective for breakthrough moments.

I am the creator of the [videocall](https://videocall.rs) project by night, but I'm also a staff engineer at May Mobility. If your autonomous vehicle trip has any sort of latency—like or interruption during the ride—that's on me.

I take this responsibility seriously, and sometimes my brain wakes me up in the middle of the night to get that one last thing done.

This article will be a guide on how to get the most out of your 2 AM coding session.

## 1. Define your goal

Realistically, you only have a few hours to work, so you need to define your goal. Be realistic, don't set yourself up for failure.

You are already working late, so it better be worth it.

## 2. Define when you are going to bed

So if you start at 2 AM, you should plan to go to bed at 3:30 AM tops. You need to sleep.

## 3. Get your environment ready

- Turn off notifications: both browser and phone: **nothing destroys focus like a Discord notification.**
- Close all other tabs.
- Turn on the lights, you'll be surprised how much better you can see.
- Contrary to the popular belief, you don't need to be in a dark room to code. In fact, you'll be surprised how much better you can see with the lights on.
- Set your perfect playlist. Use a dbmeter to get the perfect volume. I recommend 50dB - 60dB you'll probably find yourself adjusting the volume throughout the session.
- Here's my playlist if you want to use it: [Deep Focus: 2 AM](https://music.youtube.com/browse/VLPLxM2CWwQlzBtfIgvgv2lwqhNlJBPD0eLI)


## 4. Use a Pomodoro Timer

Utilize a pomodoro timer, this will help you stay focused and on track.

Here's a pomodoro timer I use: [Pomodoro Timer](https://chromewebstore.google.com/detail/pomodoro-chrome-extension/iccjkhpkdhdhjiaocipcegfeoclioejn?hl=en&pli=1) also [Pomofocus](https://pomofocus.io/) is a great option.

## Divide the session into smaller chunks and get to work

Between 2 AM and 3:30 AM, you have 90 minutes. Here's how I'd break down a real scenario:

**The Problem:** ETA service is returning 504s for 3% of requests, but only during peak hours. Users are seeing "Trip delayed" notifications when their ride is actually 2 minutes away.

**Chunk 1 (2:00-2:25): Reproduce the issue**
- Set up load testing to simulate peak traffic
- Check if it's database connection pooling or downstream service timeout
- One laser focus: "Can I reproduce this locally?"

**5-minute break: Step away from screen**
- Don't scroll social media - your brain needs to process
- Get water, look out window, let the subconscious work

**Chunk 2 (2:30-2:55): Root cause analysis**  
- Now that I can reproduce it, dive into logs
- Is it the rate limiter? Database query timeout? Network partition?
- One laser focus: "What's the actual bottleneck?"

**5-minute break: Step away from screen**
- Go to the bathroom, get water, move around.

**Chunk 3 (3:00-3:25): Minimum viable fix**
- Not the perfect solution - that's for tomorrow
- Circuit breaker pattern? Increase timeout? Graceful degradation?
- One laser focus: "What's the safest change that fixes this tonight?"

## Stop! Save your work!!

I cannot stress this enough. Stop! Commit your work!! I cannot tell you how many times due to the fact that is 3:30 AM and you're trying to get it done, you forget to save your work and you lose it all!

Write a nice commit message because tomorrow you'll forget the details and you'll have to re-read the code.

## 5. Get a good night's sleep

You need to sleep.

## 6. Repeat

## 7. Why am I planning on continue doing this?

Throughout my careers, I had many of my major breakthroughs at 2 AM, and I'm not alone.


## References

1. [Why Productivity Peaks at 2 AM](https://corner.buka.sh/why-productivity-peaks-at-2am-the-myth-the-madness-and-the-method/)

2. [Time of day effects on problem solving: When the non-optimal is optimal](https://www.researchgate.net/figure/correct-for-each-problem-solved-during-optimal-and-non-optimal-times-of-day_tbl1_254225496)
