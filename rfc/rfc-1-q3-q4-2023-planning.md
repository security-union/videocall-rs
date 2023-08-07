# RFC-1: q3-q4-2023 Planning

# I Executive Summary 

## A. Goal of the System

Creating an open video conferencing ecosystem — i.e. open source definitions and building blocks that would support video conferencing that could be embedded in various apps — as well of course as the video conferencing system itself.

For possible applications, imagine things like telemedicine apps or face-to-face customer service portals, where the video chat would be integrated rather than requiring a participant to say the equivalent of “I’ll send you a zoom link”.

The Rustlemania website is a sample implementation of this system.

## B. Purpose of the Request for Comment (RFC) 

Define our roadmap for Q3 and Q4 of 2023

## C. Overview of the Organization 

Griffin Obeid: griffobeid@securityunion.dev Co-founder and developer

Ronen Barzel: ronen@barzel.org Core Contributor

Dario Lencina: dario@securityunion.dev  Co-founder and developer

# II. Current System 

## A. Description of the existing system

`videocall-rs` aims to provide a world-class video conferencing system. The video conferencing software should always be open-source and MIT licensed. Also a key feature of this system is that all calls will be end-to-end encrypted between the peers in the call. 

The current videocall-rs exists at [https://github.com/security-union/videocall-rs](https://github.com/security-union/videocall-rs) and is hosted at [https://rustlemania.com](https://rustlemania.com). The primary connection mechanism is using WebSockets, but the system does also support WebTransport. Everything is written in Rust with the UI being a WASM based web application built through the Yew Web Framework. The UI is very barebones at this point. The backend is highly scalable by using a pub/sub architecture with NATS we can run as many instances of the server as we want. 

## B. Strengths and limitations of the current system

**Strengths**
- Highly-scalable architecture
- Written in Rust
- Using WebCodecs API
- Multiple connection types supported
- Open source & MIT licensed

**Limitations**
- Only works on Chrome or Chromium based browsers

# III. Proposed System Roadmap 

## A. Roadmap

| Quarter | Features | Freeze Date | Release Date | Release |
| ------- | -------- | ----------- | ------------ | ------- |
| Q3 2023 | E2EE | 2023-08-06 | 2023-08-08 | 2 - Alice in Chains |
| Q3 2023 | Build medical appointment react demo | 2023-08-27 | 2023-08-30 | 4 - The Alan Parsons Project |
| Q3 2023 | Safari support | 2023-09-01 | 2023-09-05 | 3 - Polyphia |
| Q3 2023 | Iggy | 2023-09-15 | 2023-09-20 | 4 - Iggy Pop |

## C. Expected outcomes and benefits

The primary goal is to determine market-fit and ensure that the products that we are building resonate with the community that we want to serve: developers and their companies.

# IV. Specific Areas for Comment 

## A. Feedback on the overall roadmap 

Feedback on the overall roadmap is essential to ensure the project is on the right path. Contributors can provide their insights on the strategic direction, scope, and timeline of the project. This can include comments on the proposed milestones, the sequencing of tasks, and the prioritization of different features or phases. Feedback can also touch on the project's alignment with broader goals or standards, as well as how it fits within the larger ecosystem of related projects.

## B. Suggestions for improvements or alternatives to proposed phases 

Contributors should feel free to propose improvements or alternatives to the plans outlined in the RFC. This can include suggesting more efficient methods to reach a phase, proposing different ways to achieve the same outcome, or pointing out potential pitfalls and offering mitigating strategies. Constructive criticism is valuable as it could lead to innovative solutions that were not initially considered, therefore enriching the diversity of the proposed solutions and increasing the chances of project success.

## C. Comments on feasibility and implementation 

Comments on feasibility are important to keep the project grounded and realistic. This involves evaluating whether the goals set are achievable within the established timeline and with the available resources. Implementation comments, on the other hand, focus more on the technical aspects, such as the practicality of proposed methods, the soundness of the architecture, and any potential technical hurdles. It also includes considerations about testing strategies, performance implications, and maintenance concerns.

## D. Thoughts on integration with existing infrastructure

Sharing thoughts on how the proposed changes can integrate with existing infrastructure is crucial. This could involve compatibility issues with existing systems, impact on workflows, necessary adaptations, or potential disruption. Consideration should also be given to how the project will interact with other tools, libraries, or systems, and how it fits into the current and future technology stack. It is vital to assess whether the proposal will enhance the existing infrastructure, create redundancies, or require substantial modifications.

# V. Feedback Submission 

## A. Format and content for feedback 

File a PR to this repo with your change proposal and the team will look at it, we can setup a call to go over the changes.

# VI. After the RFC Process 

## A. Review and incorporation of feedback 

The Security Union team commits to reviewing all the feedback and working with the contributors to advocate for their initiatives.
