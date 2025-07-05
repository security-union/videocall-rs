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


## Disclaimer

I do not recommend doing this regularly. It's not sustainable and will burn you out.

## The Why Behind Late-Night Coding

I'm a staff engineer at May Mobility. If your trip has any sort of latency—like delayed audio announcements telling you your autonomous ride has arrived—that's on me.

I take this responsibility seriously, and sometimes my brain wakes me up in the middle of the night to get that one last thing done.

## 1. Define your goal
Realistically, you only have a few hours to work, so you need to define your goal. Be realistic, don't set yourself up for failure.

You are already working late, so it better be worth it.

## 2. Define when you you are going to bed

So if you start at 2 AM, you should plan to go to bed at 3:30 AM tops. You need to sleep.

## 3. Get your environment ready

- Turn off notifications: both browser and phone: nothing destroys focus like a Discord notification.
- Close all other tabs.
- Turn on the lights, you'll be surprised how much better you can see.
- Contrary to the popular belief, you don't need to be in a dark room to code. In fact, you'll be surprised how much better you can see with the lights on.
- Set your perfect playlist. Use a dbmeter to get the perfect volume. I recommend 50dB - 60dB.
- Here's my playlist if you want to use it: [Deep Focus: 2 AM](https://music.youtube.com/browse/VLPLxM2CWwQlzBtfIgvgv2lwqhNlJBPD0eLI)


## 4. Use a pomodoro timer

Utilize a pomodoro timer, this will help you stay focused and avoid burnout.

Here's a pomodoro timer I use: [Pomodoro Timer](https://chromewebstore.google.com/detail/pomodoro-chrome-extension/iccjkhpkdhdhjiaocipcegfeoclioejn?hl=en&pli=1) also [Pomofocus](https://pomofocus.io/) is a great option.

## Divide the session into smaller chunks

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


