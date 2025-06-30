+++
title = "Shortcode Example"
date = "2024-06-14"

[taxonomies]
tags=["example"]

[extra]
comment = true
+++


## Note

Here is an example of the `note` shortcode:

This one is static!
{{ note(header="Note!", body="This blog assumes basic terminal maturity") }}

This one is clickable!
{{ note(clickable=true, hidden = true, header="Quiz!", body="The answer to the quiz!") }}


Syntax:
```
{{/* note(header="Note!", body="This blog assumes basic terminal maturity") */}}
{{/* note(clickable=true, hidden = true, header="Quiz!", body="The answer to the quiz!") */}}
```

You can also use some HTML in the text:
{{ note(header="Note!", body="<h1>This blog assumes basic terminal maturity</h1>") }}


Literal shortcode:
```
{{/* note(header="Note!", body="<h1>This blog assumes basic terminal maturity</h1>") */}}
```

Pretty cool, right?

Finally, you can do something like this (hopefully):

{% note(clickable=true, header="Quiz!") %}

# Hello this is markdown inside a note shortcode

```rust
fn main() {
    println!("Hello World");
}
```

We can't call another shortcode inside a shortcode, but this is good enough.

{% end %}

Here is the raw markdown:

```markdown
{{/* note(clickable=true, header="Quiz!") */}}

# Hello this is markdown inside a note shortcode

\`\`\`rust
fn main() {
    println!("Hello World");
}
\`\`\`

We can't call another shortcode inside a shortcode, but this is good enough.

{{/* end */}}
```

Finally, we have center
{{ note(center=true, header="Centered Text", body="This is centered text") }}

```markdown
{{/* note(center=true, header="Centered Text", body="This is centered text") */}}
```
It works good enough for me!
