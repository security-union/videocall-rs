+++
title = "Coding with AI still has flaws, but it is a force multiplier and you should use it now."
date = 2025-07-27
description = "Coding with AI still has flaws, but it is a force multiplier and you should use it now."
authors = ["Dario Lencina Talarico"]
slug = "your-brain-has-token-exhaustion-here-is-how-fix-it"
tags = ["ai", "productivity", "creativity", "time-management", "remote-work", "ai-productivity", "ai-creativity", "ai-time-management", "ai-remote-work"]
categories = ["Productivity", "Creativity", "Time Management", "Remote Work"]
keywords = ["ai", "productivity", "creativity", "time-management", "remote-work", "ai-productivity", "ai-creativity", "ai-time-management", "ai-remote-work"]
[taxonomies]
tags = ["ai", "productivity", "creativity", "time-management", "remote-work", "ai-productivity", "ai-creativity", "ai-time-management", "ai-remote-work"]
authors = ["Dario Lencina Talarico"]
+++

>   **Warning: this article is written by my pedestrian human brain and fleshy fingers, no AI was harmed in the process other than SEO optimization and read proofing.**

# Coding with AI still has flaws, but it is a force multiplier and you should use it now. 

While everyone seems to focus in a mindset of scarsity zero sum game, trying to fight AI, making fun of the obvious flaws, the 6 finger hands, the basic algebra issues, as a creator, new father and full time employee at May Mobility, AI helps me to build my dream apps with the limited amount of time I got, this includes primarily [videocall.s](https://videocall.rs) and [Remote Shutter](https://apps.apple.com/us/app/remote-shutter-camera-connect/id633274861)

Picture this, it is sunday 4:57 am, my brain won't let me sleep, I am on a mission, I want to rewrite my already successful remote shutter app, I want to incorportate tiktok/short creation, but prior to that, I need to rewrite significant chunks of the UI in SwiftUI, as many of you know, a few years ago, Apple pivoted from Storyboards to SwiftUI. It used to be the case that I needed to hire a designer, and a developer to be able to deliver new features in a timely matter, now with AI that is all out the window.

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/mom_and_baby.PNG" alt="mom and baby" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>

Even when it still needs human guidance, just like Waymos which are known to use remote assistance, AI is a force multiplier, within 10 minutes I was able to prototype a new feature, and I was able to do it with the help of AI.

I crank up the Dark Side of the Moon, because listening to AI slop like Brazilian Phonk is a step too far for this boomer. Although I listen through it through YouTube music, so my inner self is judging me, if I was still cool I would play it on my stereo. (of course I bought the physical album as I am typing, mine is in the homeland Mexico)


I open the Remote Shutter app. Look at the storyboard that I wrote more than 10 years ago, and confindently say: "I am going to tear you down motha f*cka" because as you know, all the cool kids are writing their iOS apps in SwiftUI, Storyboards are for boomer developers (which I am XD), (yes I also have thousands of Objective C lines) I am that old.

So, I select Claude 4.5 and ask:

```
I want to rewrite @MonitorViewController entirely in SwiftUI, preserve all existing functionality but modernize the look and feel. 

You'll have to modify the @MainPeer.storyboard to point to the new SwiftUI view.

Preserve localizations, and all existing functionality.

Do not dive heads first, first tell me what you think about this feature, then produce a plan, and WHEN APPROVED, then execute the plan.
```

## 💡 Pro Tip

> **Always ask AI to produce a plan, and then ask it to execute the plan.** 
> 
> Else it will poop all over your codebase, and you'll end up writing one of those bitter LinkedIn posts talking about how AI is a scam.

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/ai-bad-newb-post.png" alt="election process" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>

Anyways, we go back and forth, within 20 prompts, I was able to get a working prototype, and I am happy. Let me know if you want to see the back and forth in detail, yes, I had to type like 2 lines of code, because somehow Claude 4 forgot about how Optionals work in Swift for a second, yet it produced a beautiful working prototype that required about 600 lines of code.

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/broken-state.png" alt="election process" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>


Along of those 20 prompts I tested the prototype, if it was backend code, I would have produced unit tests, but you know XCode and friends, many times the code had regressions, so I recommend that you git commit your code often, and you can always revert to a previous version if you need to.

Working UI:

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/working-ui2.gif" alt="election process" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>

Reality check: 

It Still does not work in 1 prompt, I do not think you can prompt your way to success, but I see myself like the coach of the AI, AI has much more stamina, energy, and focus than I ever will, it does not get tired, well, its context window is limited, so I have to start from scratch often, but that is a minor nuisance compared to the benefits that it brings.

# The potential for human freedom
I have never held a leadership position, mostly because I love programming and I am really good at it, from unlocking your Corvette to buying your tickets through Ticketmaster (I know evil company) I am behind that (to so me degree there are many devs working on that stuff nowadays), but the reality is that my time is limited, my WPM (Words Per Minute) are around 100, and when I am really thinking through a problem probably it drops to 50 or less. 

My throughput is limited, AI is a force multiplier. It can turn the 2 hours session between 5 am and 7 am into 20 hours of work. 

Now I know how Managers feel, and it is freaking awesome, the idea that you can deliver 10x more than you could have done by yourself, and you can do it in a fraction of the time.






 