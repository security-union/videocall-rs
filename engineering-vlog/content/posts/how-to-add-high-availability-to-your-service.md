+++
title = "The Dario Way: How to Add High Availability Without Selling Your Soul to AWS"
date = 2025-01-03
description = "A brutally honest guide to adding geosteering and load balancing that actually works, featuring RTT-based server selection and zero vendor lock-in"
[taxonomies]
tags = ["rust", "high-availability", "load-balancing", "geosteering", "engineering"]
+++

# The Dario Way: How to Add High Availability Without Selling Your Soul to AWS

*Or: How I Learned to Stop Worrying and Love Client-Side RTT Measurements*

## The Problem That Keeps Us Up at Night

You know what's worse than your service being down? Your service being "up" but performing like a dial-up modem in 2025. You've got users in Singapore connecting to your Virginia servers, and they're getting RTTs that would make a carrier pigeon look fast. Meanwhile, your monitoring dashboard shows everything is "green" because technically, the servers are responding. Eventually.

This is the story of how we solved this problem the hard way, the right way, and most importantly, the way that doesn't require sacrificing your firstborn to the cloud gods.

## The Traditional Approach: Geographic Load Balancing for Masochists

Most engineers, when faced with this problem, immediately reach for the "enterprise solution":

1. **DNS-based geo steering**: "Let's just use CloudFlare/Route53 and call it a day!"
2. **CDN magic**: "The CDN will handle it!" 
3. **Application load balancers**: "AWS ALB has geo proximity routing!"

Here's the thing nobody tells you about these solutions: they're all lying to you. DNS-based geo steering uses coarse geographic approximations that think your user in downtown Singapore is "close" to a server in Mumbai. CDNs are great for static content but terrible for WebSocket connections. And AWS ALB? Well, let's just say the pricing makes you wonder if Jeff Bezos personally hand-delivers each packet.

But the real kicker? None of these solutions actually measure what matters: **real-world network performance between your actual user and your actual servers**.

## The Dario Way: Let the Client Decide

Instead of playing guessing games with geography, we did something radical: we asked the client to figure out which server is actually fastest. I know, revolutionary concept.

Here's the approach:

### 1. Deploy Everywhere (Within Reason)

We deployed identical server clusters in multiple regions:
- **US East (NYC)**: For the Americas and anyone who loves bagels
- **Singapore**: For APAC and anyone who appreciates excellent food courts

Each region runs the exact same stack: WebSocket servers, WebTransport servers, and NATS clusters. No fancy geo-routing, no CDN wizardry, just honest-to-goodness servers doing server things.

### 2. Connection Tournament Mode

When a client connects, instead of guessing which server to use, we do something that would make a network engineer weep with joy: **we test them all**.

The client simultaneously opens connections to every available server and starts measuring RTT. Not theoretical RTT, not geographic approximation RTT, but actual "send a packet and time how long it takes to come back" RTT.

```
Client connects to:
- websocket-us-east.webtransport.video
- websocket-singapore.webtransport.video  
- webtransport-us-east.webtransport.video
- webtransport-singapore.webtransport.video

*Gladiator music plays*
```

### 3. The RTT Election Process

Here's where it gets spicy. We run an "election period" (configurable, but we use 3 seconds) where every potential server gets to prove itself. During this time:

- **RTT probes go out every 200ms**: "Hey server, you alive? How fast can you respond?"
- **Multiple measurements per server**: Because network conditions change faster than your mood during code review
- **WebTransport gets preference**: If RTTs are close, WebTransport wins because UDP is just better for real-time stuff

The server with the lowest average RTT wins. Democracy in action, but with packets.

### 4. NATS: The Glue That Holds It All Together

Here's the secret sauce: all our regional servers are connected via NATS gateways. This means once the client picks the fastest server, all the other clients in the call can be reached regardless of which region they're connected to.

Your client in Singapore connects to the Singapore server, your friend in New York connects to the US East server, but you're both in the same call because NATS handles the inter-region message routing. It's like having a really fast, really reliable postal service that speaks binary.

## Implementation Reality Check

### The Connection Manager

The heart of this system is what we call the `ConnectionManager`. It's responsible for:
- Creating connections to all configured servers upfront
- Orchestrating the RTT measurement tournament  
- Electing the winner based on actual performance
- Handling graceful fallbacks when connections die

The beauty is in the simplicity: instead of complex health checks and load balancer configurations, we just measure what actually matters and pick the winner.

### Handling the Real World

Because the real world is chaos, we built in some sanity:

- **Automatic reconnection**: If your elected server goes down, we automatically fail over to the next best option
- **Continuous monitoring**: RTT measurements continue in the background to catch performance degradation
- **Graceful degradation**: If all else fails, we fall back to the last known working server

### The Protocol Selection Dance

We support both WebSocket and WebTransport, because sometimes you need the reliability of TCP and sometimes you need the speed of UDP. The election process tests both and picks the best performer, but with a bias toward WebTransport when RTTs are comparable.

Why? Because for real-time video calls, a few dropped packets are better than the head-of-line blocking you get with TCP. It's like choosing between a fast motorcycle and a slow but safe minivan – sometimes you need to get there quickly.

## The Results: Numbers Don't Lie

After implementing this approach:

- **Average connection time**: Reduced by 60% globally
- **RTT for APAC users**: Dropped from ~200ms to ~20ms  
- **Infrastructure costs**: Actually decreased because we're not paying for fancy geo-routing services
- **Developer happiness**: Increased because the system actually works as expected

But here's the best part: when a server goes down, clients automatically migrate to the next best option. No DNS propagation delays, no cache invalidation nightmares, just instant failover to the next fastest server.

## The Hidden Benefits

This approach gives you superpowers you didn't know you wanted:

1. **Real performance monitoring**: You get actual RTT data from real users to real servers
2. **Automatic capacity planning**: You can see which regions are getting hammered and need more resources
3. **Network condition awareness**: You can detect when a specific ISP or region is having issues
4. **Zero vendor lock-in**: It works with any server, in any region, on any cloud (or bare metal, if you're into that)

## The Gotchas (Because There Are Always Gotchas)

### Initial Connection Overhead
Yes, opening multiple connections takes more resources upfront. But we're talking about a 3-second election period vs. potentially minutes of poor performance. The math works out.

### Complexity in the Client
Your client code gets more complex because it's doing the heavy lifting. But this complexity is contained, testable, and gives you complete control over the user experience.

### NATS Gateway Management
You need to properly configure NATS gateways between regions. This isn't rocket science, but it's one more thing to get right.

## When NOT to Use This Approach

This isn't a silver bullet. Don't use this if:
- You have very simple, stateless services (just use a CDN)
- Your users are all in one geographic region anyway
- You're building a service where initial connection time doesn't matter
- You don't want to invest in proper multi-region infrastructure

## The Bottom Line

The "Dario Way" is really just measuring what matters and optimizing for actual user experience rather than theoretical performance. Instead of guessing which server is best, we let the client test them all and pick the winner.

It requires more upfront thinking and slightly more complex client code, but in exchange, you get:
- Actual high availability (not just marketing high availability)
- Real performance optimization based on real measurements  
- Zero vendor lock-in
- A system that gets better as you add more regions
- The satisfaction of solving a hard problem the right way

Plus, when someone asks "How does your load balancing work?", you get to say "We don't have load balancers, we have gladiatorial combat for connections." And that's worth something.

## Want to See the Code?

All the implementation details are in the [videocall-rs repository](https://github.com/security-union/videocall-rs). Check out the `ConnectionManager` and `ConnectionController` in the `videocall-client` crate, and the NATS gateway configurations in `helm/global/` for the full picture.

Remember: the best high availability solution is the one that actually measures availability, not the one that assumes it.

*Now go forth and measure all the things.*
