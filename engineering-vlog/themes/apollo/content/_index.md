+++
template = "homepage.html"
+++

<style>
.homepage-hero {
    text-align: center;
    padding: 2rem 0;
}

.homepage-hero-title {
    font-size: 3rem;
    margin-bottom: 1rem;
}

.homepage-hero-subtitle {
    font-size: 1.25rem;
    margin-bottom: 1rem;

</style>

<div class="homepage-hero">
    <h1 class="homepage-hero-title">Apollo</h1>
    <p class="homepage-hero-subtitle">A modern and minimalistic blog theme powered by Zola.</p>
</div>

# Features

- [Light, dark, and auto themes](/posts/configuration#theme-mode-theme)  
- [Projects page](/projects/)                                 
- [Talks page](https://not-matthias.github.io/talks/)         
- [Analytics (GoatCounter, Umami)](/posts/configuration#analytics)                                                                                                  
- [Social media links](/posts/configuration#socials)                                                                                                     
- [MathJax rendering](/posts/math-symbol/)                    
- [Taxonomies](/tags/)                                 
- [Custom homepage](/posts/custom-homepage/)                  
- [Comments](/posts/configuration#comments-comment)                   
- [Search functionality](/posts/configuration#search-build-search-index)         

Checkout all the [options you can configure](./content/posts/configuration.md) and the [example pages](./content/posts/).

# Quick Start

1.  **Add the theme as a submodule:**
    ```bash
    git submodule add https://github.com/not-matthias/apollo themes/apollo
    ```
2.  **Configure your `config.toml`:**
    Set `theme = "apollo"` and add your site's configuration.
3.  **Start the Zola server:**
    ```bash
    zola serve
    ```
