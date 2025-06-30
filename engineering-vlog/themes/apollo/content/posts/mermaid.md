+++
title = "Mermaid Example"
date = "2024-12-26"

[taxonomies]
tags=["example"]

[extra]
comment = true
+++

This Theme supports [mermaid](https://mermaid.js.org/) markdown diagram rendering.

To use mermaid diagrams in your posts, see the example in the raw markdown code.
https://raw.githubusercontent.com/not-matthias/apollo/refs/heads/main/content/posts/mermaid.md

## Rendered Example

{% mermaid() %}
graph LR
    A[Start] --> B[Initialize]
    B --> C[Processing]
    C --> D[Complete]
    D --> E[Success]

    style A fill:#f9f,stroke:#333
    style E fill:#9f9,stroke:#333
{% end %}
