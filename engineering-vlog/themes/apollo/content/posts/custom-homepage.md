+++
title = "Creating a Custom Homepage"
date = 2025-06-29
+++

The Apollo theme provides a default homepage that lists your recent blog posts. However, you might want to create a custom homepage that better reflects your personality and work. This guide will walk you through the process of creating a custom homepage with the Apollo theme.

## 1. Create a Custom Homepage Template

The first step is to create a custom homepage template. In the root of your Zola project, create a new file at `templates/home.html`. This file will contain the HTML for your custom homepage.

You can use the following code as a starting point:

```html
{% extends "base.html" %}

{% block main_content %}
    <main>
        <article>
            <section class="body">
                <h1>Welcome to my custom homepage!</h1>
                <p>This is where you can introduce yourself and your work.</p>
            </section>
        </article>
    </main>
{% endblock main_content %}
```

This template extends the theme's `base.html` template and overrides the `main_content` block with your own content.

## 2. Set the Homepage Template

Next, you need to tell Zola to use your custom homepage template. In the `content` directory of your Zola project, you should have a `_index.md` file. If you don't have one, create one.

In this file, add the following front matter:

```toml
+++
template = "home.html"
+++
```

This tells Zola to use the `templates/home.html` file to render your homepage.

## 3. Add Content to Your Homepage

Now you can add content to your homepage. The content of the `content/_index.md` file will be available in your `templates/home.html` template as the `section` variable.

For example, you can add a title and some introductory text to your `content/_index.md` file:

```toml
+++
title = "Hey there! 👋🏼"
template = "home.html"
+++

I'm a software engineer who loves to write about technology and programming.
```

You can then display this content in your `templates/home.html` template:

```html
{% extends "base.html" %}

{% macro home_page(section) %}
    <main>
        <article>
            <section class="body">
                {{ post_macros::page_header(title=section.title) }}
                {{ section.content | safe }}
            </section>
        </article>
    </main>
{% endmacro home_page %}

{% block main_content %}
    {{ self::home_page(section=section) }}
{% endblock main_content %}
```

## 4. Displaying Posts

You can also display a list of your recent posts on your homepage. The following code shows how to display the 5 most recent posts:

```html
{% extends "base.html" %}

{% macro home_page(section) %}
    <main>
        <article>
            <section class="body">
                {{ post_macros::page_header(title=section.title) }}
                {{ section.content | safe }}
            </section>
        </article>
    </main>
{% endmacro home_page %}

{% block main_content %}
    {{ self::home_page(section=section) }}

    <h1>Recent articles</h1>
    <main class="post-list">
        {% set section = get_section(path="posts/_index.md") %}
        {{ post_macros::list_posts(pages=section.pages | slice(end=5)) }}
    </main>
{% endblock main_content %}
```

This code gets the `posts` section and then uses the `post_macros::list_posts` macro to display the 5 most recent posts.

You can also highlight specific posts by getting them by their path:

```html
{% set highlights = [
    get_page(path="posts/my-first-post.md"),
    get_page(path="posts/my-second-post.md"),
] %}
<main class="post-list">
    {{ post_macros::list_posts(pages=highlights) }}
</main>
```

This is just a starting point. You can customize your homepage as much as you want.
