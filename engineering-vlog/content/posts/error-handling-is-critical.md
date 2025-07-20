+++
title = "Error Handling is Critical: How Delta's Lying Machines Taught Me Everything Wrong About UX"
date = 2025-07-20
description = "A Delta airport payment disaster reveals why honest error messages and proper UX design aren't just nice-to-have features—they're the difference between functional software and digital chaos. Learn from Three Mile Island's design failures."
authors = ["Dario Lencina Talarico"]
slug = "error-handling-critical-delta-ux-disaster"
tags = ["error-handling", "ux-design", "software-engineering", "user-experience", "payment-systems", "three-mile-island", "system-design", "frontend-development"]
categories = ["Software Engineering", "UX Design", "System Design"]
keywords = ["error handling", "error messages", "UX design", "user experience", "software engineering", "payment processing", "system feedback", "Three Mile Island", "Delta airlines", "user interface design", "frontend development", "software reliability"]

# Social media meta tags
[extra]
og_title = "Error Handling is Critical: How Delta's Lying Machines Taught Me Everything Wrong About UX"
og_description = "A hilarious yet instructive tale of how Delta's payment system charged me 19 times while lying about it—and what software engineers can learn from Three Mile Island about honest error messages."
og_image = "/images/deltafail.jpg"
og_type = "article"
twitter_card = "summary_large_image"
twitter_title = "Error Handling is Critical: Delta's UX Disaster Story"
twitter_description = "How I accidentally paid $200 for one checked bag because Delta's machines lie about their status—and what Three Mile Island teaches us about honest error messages."
twitter_image = "/images/deltafail.jpg"
reading_time = "8"
+++

# Error Handling is Critical: How Delta's Lying Machines Taught Me Everything Wrong About UX

*Or: How I Accidentally Funded Delta's Quarterly Earnings While My Mother-in-Law Almost Missed Her Flight*

## The Setup: I do not like to wake up early.

My sister-in-law wanted us to leave for the airport at 5:30 PM for a 9:00 PM flight. "That's excessive," I thought, like the arrogant software engineer I am. "We can optimize this." Well, turns out she was right, and if we hadn't left when she insisted, we wouldn't have made it. Sometimes the non-technical people in your life know things about buffer time that you don't. Write that down.

But this story isn't about time management. It's about what happened when we finally got to the airport and I encountered the most spectacularly broken piece of software I've seen since... well, since the last time I flew Delta.

## The Machine That Couldn't Tell the Truth

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/deltafail.jpg" alt="Delta fail" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>

Picture this: My mother-in-law needs to pay for her checked bag. Simple, right? We walk up to one of those self-service kiosks that looks like it was designed by someone who's never actually used a credit card. The interface is asking for payment, so I swipe my Amex.

**"YOUR CREDIT CARD COULD'T BE READ"**

Okay, weird. Maybe I swiped too fast. Let me try again.

**"YOUR CREDIT CARD COULD'T BE READ"**

Huh. Maybe the magnetic stripe is worn. Let me try inserting it.

**"YOUR CREDIT CARD COULD'T BE READ"**

Maybe this particular machine is broken? There's another kiosk right next to it. Let me try that one. Same exact thing. Card swipe, error message, repeat. Different machine, same lies.

At this point I'm thinking, "You know what? Let me find an actual human being." So I walk over to the Delta counter and explain the situation. The agent looks at me like I'm some kind of Luddite and says, "Sir, you need to use the self-service kiosk." I explain that the kiosk isn't working. "The kiosks work fine, sir. Just try again." 

This is exactly why we're not ready to replace all workers with automation—because when the automation inevitably breaks, the humans have forgotten how to do their jobs and just point you back to the broken robot. It's like tech support hell, but with baggage fees.

So I'm back at the kiosk, defeated. Fine, maybe my Amex is actually having issues. Let me try a different card. I pull out my Amazon Visa. Same error. Huh. Maybe that one's broken too? Let me try my Costco card. Same error. Okay, what about my Chase debit card? Same damn error.

Meanwhile, my iPhone is lighting up like a Christmas tree with payment notifications. 

That's when I realized what was happening. The machine wasn't having trouble reading my cards. It was reading them just fine. It was charging them perfectly. It was just *lying to me about it*.

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/deltafail2.jpg" alt="Delta fail" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>

## The $665 Lesson in Why Error Messages Matter

When the dust settled, I had:
- 10 charges on my Amex
- 3 charges on my Amazon Visa  
- 3 charges on my Costco card
- 3 charges on my Chase debit card

That's 19 transactions for one checked bag. Nineteen. I basically funded Delta's Q4 numbers single-handedly.

But here's the thing that really gets me: **This wasn't a bug. This was a design choice.**

Someone, somewhere, made the decision that when a payment processes successfully but the system can't confirm it immediately, the appropriate user message is "Unable to read card" instead of "Payment processing, please wait" or "Transaction in progress" or literally anything honest.

## The Customer Service Disaster

So I find a Delta attendant to help sort this out. Big mistake. She immediately gets defensive, like I'm personally attacking her for the machine's inability to tell the truth. 

But then I show her my Delta Reserve card (mistake #2), and suddenly she's not focused on my $200 problem anymore. Instead, she's explaining to me—while my mother-in-law is standing there with a ticking clock to her gate—that Delta employees don't get SkyClub access with Reserve cards.

Lady, I don't care about your employee benefits right now. I care about the fact that your machine just committed credit card fraud 19 times while lying about it.

At that point, I'm thinking, "You know what? I'm not going to get anywhere with this person who's apparently more interested in discussing employee benefits than actual customer service." So I gave up and got into the international check-in line, where I waited 30 minutes for a human to do in 30 seconds what their "perfectly functioning" machines had spent 20 minutes failing to do while secretly charging me for the privilege. 

The agent who finally helped us was this jolly guy who looked exactly like Santa Claus—beard, belly, the works. He took one look at our situation, typed for literally 10 seconds, and boom: bag checked, boarding pass printed. No drama, no lies, no surprise credit card charges. It was almost like he was actually trained to help customers instead of just pointing them toward malfunctioning robots. Revolutionary concept at Delta, apparently.

## The Three Mile Island of Payment Processing

<p style="text-align:center; margin-top:1em; margin-bottom:1em;">
    <img src="/images/1adfe4f1-e361-47e2-bb73-2d4131ddd1a1.webp" alt="Delta fail" style="max-width:600px; width:100%; height:auto; border-radius:4px;" />
</p>

This reminds me of the Three Mile Island disaster. Not because of the severity (though my credit card statements might disagree), but because of the exact same root cause that nearly melted down a nuclear reactor: **lying status indicators**.

At Three Mile Island, operators received confusing and contradictory information from their instruments. Here's the kicker: **The light on the control panel indicated whether the command to close a valve had been sent, not whether the valve actually closed.** This led operators to wrongly assume the valve was closed when it was not, allowing coolant to escape.

Read that again. The system showed "valve closed" when what it really meant was "we told the valve to close." Sound familiar?

The operators made rational decisions based on what their instruments told them. When the light said "closed," they assumed it was closed. When coolant kept draining, they couldn't understand why. The instruments were lying, and the operators paid the price for trusting them.

At Delta's bag check kiosk, I made the same rational decisions based on what the screen told me. When it said "unable to read card," I assumed it couldn't read the card. When charges kept appearing on my phone, I couldn't understand why. The machine was lying, and I paid the price—19 times over.

**The pattern is identical: Systems that lie about their state cause humans to take actions that make the problem worse.**

In both cases, the technical system was doing one thing while reporting another. The difference? At Three Mile Island, we nearly had a nuclear disaster. At Delta, I nearly had a cardiac event when I saw my credit card statements.

But here's what really pisses me off: **We learned this lesson 45 years ago.** Nuclear plant operators now have indicators that show actual valve position, not just command status. Meanwhile, Delta's payment systems are still stuck in 1979, showing "command sent" while pretending it means "command failed."

*For more on how Three Mile Island became the most instructive design failure in American history, check out [Google Design's deep dive](https://design.google/library/user-friendly) into how the control panel's misleading feedback nearly caused a nuclear meltdown.*

## Why This Matters for Software Engineers

Here's what every software engineer needs to understand: **Your error messages are not just text. They are instructions for human behavior.**

When you show "Unable to read card," you're telling the user to try again. When the real situation is "Payment successful but confirmation delayed," you're basically programming them to accidentally commit fraud.

This isn't just bad UX. This is dangerous UX. It's the difference between:

```
// Bad: Lying to the user
if (payment_status == PENDING) {
    show_error("Unable to read card");
}

// Good: Telling the truth
if (payment_status == PENDING) {
    show_message("Payment processing, please wait...");
    disable_payment_button();
}
```

## The Engineering Principles We Should Live By

1. **Error messages should describe reality, not hide it**: If something succeeded, don't say it failed. If something is in progress, don't say it's broken.

2. **Prevent user actions that will cause problems**: If a payment is processing, disable the payment button. This is UX 101.

3. **Test your error states**: I guarantee nobody at Delta ever tested what happens when you have multiple pending payments. Because if they had, they would have seen this disaster coming.

4. **Design for the stressed user**: Nobody uses your software in ideal conditions. They're tired, rushed, and dealing with their mother-in-law's luggage. Design for that reality.

## The Bigger Picture

This isn't really about Delta. It's about the fact that as software engineers, we have enormous power over people's lives, and most of us treat error handling like an afterthought.

We spend weeks perfecting the happy path and five minutes on what happens when things go wrong. But here's the thing: **things always go wrong**. Your users will remember how your software behaved during the failure, not during the success.

## The Conclusion (Or: How to Not Be Delta)

My mother-in-law made her flight, by the way. Barely. And I eventually got most of my money back, after enough phone calls to Delta's customer service to qualify for frequent caller status.

But the lesson here isn't about airline incompetence (though there's plenty of that). It's about the responsibility we have as engineers to be honest about what our systems are doing.

Your error messages matter. Your system state matters. The truth matters.

And if you ever find yourself writing code that lies to users about what's happening, just remember: somewhere out there, there's a guy who paid $200 for a checked bag because your error message told him to keep trying.

Don't be that engineer. Be better than Delta.

---

*P.S. - If you work at Delta and want to discuss your payment processing architecture, I'm available for consulting. I accept payment in the form of SkyMiles, preferably not charged to my credit card 19 times.*
