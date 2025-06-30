+++
title = "Extreme Ownership at 01:00 AM: Confessions of a Staff Engineer Building Autonomous Vehicle Backends"
date = 2025-06-30
# Set to `true` while drafting; switch to `false` once published
draft = false
slug = "extreme-ownership-staff-engineer"
description = "A raw look at backend reliability, observability, and leadership through the lens of extreme ownership from a staff software engineer at May Mobility who architects real-time ETA services for Lyft and Uber integrations."
tags = ["extreme ownership", "staff engineer", "software reliability", "backend architecture", "observability", "rust", "autonomous vehicles", "leadership", "devops"]
authors = ["Dario Lencina Talarico"]

[extra]
seo_keywords = ["senior engineer", "software reliability", "backend architecture", "autonomous vehicles", "rust", "observability", "may mobility", "pagerduty"]

[taxonomies]
tags = ["extreme ownership", "staff engineer", "software reliability", "backend architecture", "observability", "rust", "autonomous vehicles", "leadership", "devops"]
authors = ["Dario Lencina Talarico"]
+++

*Extreme ownership transforms invisible risk into disciplined personal action—and it's the only guarantee that a real-time autonomous-vehicle backend survives the night.*

## 01:00 AM — Sensing Invisible Risk

The cursor blinks in the darkness, matching the soft blue LED of the baby monitor two rooms away. My six-week-old daughter shuffles in her sleep, my wife breathes shallowly—still recovering from the nine-month marathon that preceded the birth—yet here I am, eyes fixed on a terminal that refuses to rest. May Mobility's backend, the one I own end-to-end, is nocturnal by nature. Every ninety seconds Lyft and Uber refresh dashboards that trace red, green, or amber back to something I designed, something I risked.

Tonight the risk is quiet, and the air already tastes of burnt coffee. That is rarely comforting. A Grafana latency panel flashes yellow, then green again before my screenshot shortcut lands. Quiet means an incident may already be unfolding unseen, a packet loss spike hiding between Prometheus scrapes. Extreme ownership—Jocko Willink's stark mantra—turns that possibility into personal debt. If the SLA slips, if the tele-assist feed stutters while a shuttle navigates downtown chaos, the fault line runs through me, not the codebase. Ownership liberates: once everything is my fault, excuses evaporate and only action remains.

## Engineering Predictability — Designing the ETA Service

The ETA service began exactly like this—under dimmed desk lamp, caffeine gone cold, baby kicking inside a still-pregnant wife trying to sleep through the clacking keys. Even tonight, a lonely Redis cache miss crackles in the logs like distant thunder—harmless, perhaps, or a match searching for tinder. The requirement sounded innocent: "Give riders an accurate arrival time." Reality laughed. GPS drifts in urban canyons, passengers judge minutes like hours. I reached for Rust, for Kalman filters fusing LiDAR ground truth, for gRPC because protobuf feels like honesty at the wire. I wrapped the whole thing in a fortress of metrics: request latency histograms, Redis cache hit ratios, downstream timeout counts. Observability first or perish.

Two hundred milliseconds—less time than it takes neon rain to kiss the pavement. In that breath, the Ego (autonomous vehicle) coughs up its prophecy, sensors contest, algorithms arbitrate. The verdict leaves my service as a single number, pulsing across the wire toward Lyft's unforgiving gateway. Miss by a heartbeat and the ride evaporates. Passengers seethe, drivers snap, metrics bleed red. Anger spiders upward: C-suite jaws tighten, stock tickers sputter, investors flick to other channels. In the ashes of a bad ETA, a whole company can die without firing a shot.

The first full test run finished at 03:00 AM on a Tuesday I now remember only by the smell of burnt coffee. Weeks stretched into months until I pushed `v1.0.0` behind a feature flag. The staged rollout produced zero regressions and exactly one silent thank-you—from the universe, perhaps. At my level, nobody applauds when systems work; working is the default, the bare minimum. I'm fine with that. My reward is the flatness of graphs, the compounding of stock options into potential generational wealth.

## 04:12 AM — Incident and Response

04:12 AM. PagerDuty screams: *Framerate < 5 fps on Miranda. Verify cellular connection.* My heart delivers its own alert: if operators can't see the road clearly, the shuttle will safe-stop in five seconds and dozens of commuters will mutter about unreliable tech. 99.999 percent reliability feels impressive until you are staring at the 0.001 percent in real time. The cause—undocumented ISP maintenance—was mundane; the consequence was not. By 04:30 AM I had forced a fallback to our secondary LTE path, throttled encoder bitrate, and filed a retro ticket before most of the company's Slack status lights turned green.

```text
04:12 ▸ PagerDuty page — Miranda video <5 fps
04:13 ▸ Executed failover script — switched to secondary carrier
04:20 ▸ Throttled encoder bitrate to 1 Mbps
04:30 ▸ Filed retro ticket and opened root-cause thread
```

> `ops-miranda 04:12`  Mirada's front cam is toast—seeing artifacts.

Failure still stings, but it fuels the backlog. We now heartbeat each Peplink connection, visualize bit errors beside weather radar overlays, and test failovers twice per sprint. Ownership means turning shame into system tests.

## Alliances and the Road Ahead

Partnerships aren't API contracts; they're trust ledgers. Every time Lyft suggests a feature and we answer "already live," the ledger tilts in our favor. The north star is predictable: integrations → precedents → partnerships → platform dominance. The route is not. But I will navigate it, one pull request, one alert, one caffeine-stained sunrise at a time.

## 06:00 AM — Dawn and Call to Action

The alert dashboard is green again. Outside, morning bleeds through the blinds. My daughter stirs, my wife smiles in semi-sleep, and the system hums without complaint. I failed last night, learned by dawn, and shipped the fix before breakfast.

Pressure vents better through prose than through bourbon. I write to remind May Mobility—and myself—that reliability is intentional, never accidental. I write so junior engineers can see that ownership scales with scope, but so does fulfillment. If you've read this far, pick one alert—any alert—and own it before midnight strikes. Will your system hum at dawn?

That is enough—for now. Time to go to work.

## About the Author

*Dario Lencina Talarico is a Staff Software Engineer at May Mobility, where he architects and operates the real-time backends that keep autonomous shuttles safe and on schedule. Side projects like Videocall.rs—and this engineering vlog—are his playground for exploring ultra-low-latency systems at the edge.*
