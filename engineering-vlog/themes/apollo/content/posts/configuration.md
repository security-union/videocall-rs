+++
title = "Configuring Apollo"
date = "2024-07-09"

[taxonomies]
tags=["documentation"]

[extra]
repo_view = true
comment = true
+++

# Site Configuration

## Search (`build_search_index`)

Enables or disables the search functionality for your blog.

- Type: Boolean
- Default: false
- Usage: `build_search_index = false`

When enabled, a search index will be generated for your blog, allowing visitors to search for specific content.
Additionally, a search button will be displayed in the navigation bar.

Configure the search like this:

```toml
build_search_index = true

[search]
include_title = true
include_description = true
include_path = true
include_content = true
index_format = "elasticlunr_json"
```

## Theme Mode (`theme`)

Sets the color theme for your blog.

- Type: String
- Options: "light", "dark", "auto", "toggle"
- Default: "toggle"
- Usage: `theme = "toggle"`

The "toggle" option allows users to switch between light and dark modes, while "auto" typically follows the user's system preferences.

## Menu

Defines the navigation menu items for your blog.

- Type: Array of objects
- Default: []
- Usage:
  ```toml
  menu = [
    { name = "/posts", url = "/posts", weight = 1 },
    { name = "/projects", url = "/projects", weight = 2 },
    { name = "/about", url = "/about", weight = 3 },
    { name = "/tags", url = "/tags", weight = 4 },
  ]
  ```

## Logo

Defines the site logo image file.

- Type: String
- Usage:
  ```toml
  logo = "site_logo.svg"
  ```

## Socials

Defines the social media links.

- Type: Array of objects
- Default: []
- Usage:
  ```toml
  socials = [
    { name = "twitter", url = "https://twitter.com/not_matthias", icon = "twitter" },
    { name = "github", url = "https://github.com/not-matthias/", icon = "github" },
  ]
  ```

## Table of Contents (`toc`)

Enables or disables the table of contents for posts.

- Type: Boolean
- Default: true
- Usage: `toc = true`

When enabled, a table of contents will be generated for posts, making it easier for readers to navigate through longer articles.

Note: This feature adds additional JavaScript to your site.

## CDN Usage (`use_cdn`)

Determines whether to use a Content Delivery Network (CDN) for assets.

- Type: Boolean
- Default: false
- Usage: `use_cdn = false`

When set to true, the theme will attempt to load assets from a CDN, which can improve loading times for visitors from different geographic locations.

## Favicon (`favicon`)

Specifies the path to the favicon image for your blog.

- Type: String
- Default: "/icon/favicon.png"
- Usage: `favicon = "/icon/favicon.png"`

This sets the small icon that appears in the browser tab for your website.

## Custom Stylesheets (`stylesheets`)

Allows you to add custom stylesheets to your blog.

- Type: Array of files located in the `static` directory
- Default: []
- Usage:
  ```toml
  stylesheets = [
    "custom.css",           # static/custom.css
    "/css/another.css"      # static/css/another.css
  ]
  ```

## Fancy Code Styling (`fancy_code`)

Enables enhanced styling for code blocks.

- Type: Boolean
- Default: true
- Usage: `fancy_code = true`

This option adds the language label and a copy button.

## Dynamic Notes (`dynamic_note`)

Allows for the creation of togglable note sections in your content.

- Type: Boolean
- Default: true
- Usage: `dynamic_note = true`

When enabled, you can create expandable/collapsible note sections in your blog posts.

## Anchor Links

You can add anchor links by adding the following to your `_index.md`:

```toml
insert_anchor_links = "heading"
```

## Tanonomy sorting

You can sort the taxonomies page with the following config:
```toml
[extra.taxonomies]
sort_by = "page_count"         # e.g. name, page_count
reverse = true
```

The `sort_by` argument is directly passed to the `sort_by` function:
```jinja
{% set sort_by = config.extra.taxonomies.sort_by | default(value="name") %}
{% set terms = terms | default(value=[]) | sort(attribute=sort_by) %}

{% if config.extra.taxonomies.reverse | default(value=false) %}
    {% set terms = terms | reverse %}
{% endif %}

{% for term in terms %}
    <li>
        <a href="{{ term.permalink | safe }}">
            {{ term.name }} ({{ term.pages | length }} post{{ term.pages | length | pluralize }})
        </a>
    </li>
{% endfor %}
```

Possible values include anything within the [TaxonomyTerm object](https://www.getzola.org/documentation/templates/taxonomies/):
```rust
name: String;
slug: String;
path: String;
permalink: String;
pages: Array<Page>;
page_count: Number;
```

Examples:
- `name` to sort by name
- `page_count` to sort by page count

## Analytics

Enable or disable analytics tracking:

```toml
[extra.analytics]
enabled = false
```

After enabling analytics, configure GoatCounter or Umami.

### GoatCounter

Configure GoatCounter analytics:

```toml
[extra.analytics.goatcounter]
user = "your_user"           # Your GoatCounter username
host = "example.com"         # Optional: Custom host
```

### Umami Analytics

Configure Umami analytics:

```toml
[extra.analytics.umami]
website_id = "43929cd1-1e83...."                    # Your Umami website ID
host_url = "https://stats.mywebsite.com"            # Optional: Custom host URL
```

---

# Page configuration

## Source code (`repo_view`)

Do you want to link to the source code of your blog post? You can turn on the `repo_view` inside the `[extra]` section of your blog post.

```toml
[extra]
repo_view = true
repo_url = "https://github.com/not-matthias/apollo/tree/main/content" # Alternatively add the repo here
```

The `repo_url` can be set in the `[extra]` section or in your `config.toml`.

## Comments (`comment`)

Enables or disables the comment system for posts.

- Type: Boolean
- Default: false
- Usage: `comment = false`

After making `comment = true` in `[extra]` section of you post, save your script from [Giscus](https://giscus.app) to `templates/_giscus_script.html`.
When enabled, this allows readers to leave comments on your blog posts. This feature has to be set for each individual post and is not supported at higher levels.

Example configuration in [content/posts/configuration.md](https://github.com/not-matthias/apollo/blob/main/content/posts/configuration.md):

```toml
+++
title = "Configuring Apollo"

[extra]
comment = true
+++
```

Comments via [utterances](https://utteranc.es) can be configured in `template/_giscus_script.html` like this:

```html
<script src="https://utteranc.es/client.js"
        repo="YOUR_NAME/YOUR_REPO"
        issue-term="pathname"
        theme="github-light"
        crossorigin="anonymous"
        async>
</script>
```

# Cards Page

The `cards.html` template allows you to display a list of items in a card format. This is ideal for showcasing projects, but can be used for any list of items you want to display in a visually appealing way.

To create a cards page, you need to create a `_index.md` file in a content directory (e.g., `content/projects`). The following front matter is recommended:

```toml
+++
title = "Projects"
sort_by = "weight"
template = "cards.html"
+++
```

Each item in the list should be a separate markdown file in the same directory. The following front matter is supported:

- `title`: The title of the item.
- `description`: A short description of the item.
- `weight`: The order in which the item appears on the page.
- `local_image`: A path to a local image for the item's thumbnail. See the [Local Image](#local-image) section for more details.
- `link_to`: A URL the card should link to.

# Talks Page

To create a talks page, you need to create a `_index.md` file in the `content/talks` directory. The following front matter is recommended:

```toml
+++
title = "Talks"
sort_by = "date"
template = "talks.html"
+++
```

Each talk should be a separate markdown file in the `content/talks` directory. The following front matter is supported:

- `title`: The title of the talk.
- `description`: A short description of the talk.
- `local_image`: A path to a local image for the item's thumbnail. See the [Local Image](#local-image) section for more details.
- `date`: The date of the talk.
- `video`: A map with a `link` and `thumbnail` for the talk video.
- `organizer`: A map with a `name` and `link` for the event organizer.
- `slides`: A URL to the presentation slides.
- `code`: A URL to the source code.

# Local Image

The `local_image` front matter parameter allows you to specify a path to a local image that will be used as the thumbnail for a page. This is particularly useful for social media previews and other places where a representative image is needed.

The path resolution for `local_image` works as follows:

- If the path starts with a `/`, it is treated as an absolute path from the `content` directory. For example, `local_image = "/projects/project-1.jpg"` will resolve to `content/projects/project-1.jpg`.
- If the path does not start with a `/`, it is treated as a relative path. The theme will prepend the `section.path` to the `local_image` path. For example, if you are in a page at `content/posts/my-post/index.md` and you set `local_image = "thumbnail.png"`, the theme will look for the image at `posts/my-post/thumbnail.png`.
