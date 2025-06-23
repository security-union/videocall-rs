/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */


use leptos::*;
use leptos_router::ActionForm;

#[server(SaveFavorites, "/api")]
pub async fn save_favorites(
    favorite_cookie_type: String,
    favorite_color: String,
) -> Result<String, ServerFnError> {
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    Ok(format!(
        "Here, have some {favorite_color} {favorite_cookie_type} cookies!"
    ))
}

pub const EXAMPLE_SERVER_FUNCTION_CODE: &str = r#"```rust
#[server(SaveFavorites, "/api")]
pub async fn save_favorites(
    favorite_cookie_type: String,
    favorite_color: String,
) -> Result<(), ServerFnError> {
    let pool = get_pool()?;

    let query = "
        INSERT INTO COOKIES 
        (favorite_cookie_type, favorite_color)
        VALUES ($1, $2)
    ";

    sqlx::query(query)
        .bind(favorite_cookie_type)
        .bind(favorite_color)
        .execute(&pool)
        .await
        .map_err(|e| 
            ServerFnError::ServerError(e.to_string())?;

    Ok(format!("Here, have some {favorite_color} {favorite_cookie_type} cookies!"))
}

#[component]
pub fn FavoritesForm() -> impl IntoView {
    let favorites = create_server_action::<SaveFavorites>();
    let value = favorites.value();
    view! { 
        <ActionForm action=favorites>
            <label>
                "Favorite type of cookie"
                <input
                    type="text"
                    name="favorite_cookie_type"
                />
            </label>
            <label>
                "Favorite color"
                <input
                    type="text"
                    name="favorite_color"
                />
            </label>
            <input type="submit"/>
        </ActionForm>
        <Show when=favorites.pending()>
            <div>"Loading..."</div>
        </Show>
        <Show when=move || value.with(Option::is_some)>
            <div>{value}</div>
        </Show>
    }
}
```"#;

#[island]
pub fn ExampleServerFunction() -> impl IntoView {
    let favorites = create_server_action::<SaveFavorites>();
    let value = favorites.value();
    view! {
        <ActionForm action=favorites>
            <div class="p-4 sm:p-8">
                <h2 class="text-2xl font-bold text-black dark:text-eggshell">"Save to database"</h2>
                <div class="my-4">
                    <div class="flex flex-col gap-4">
                        <div>
                            <label
                                for="favorite_cookie_type"
                                class="block text-sm font-bold text-black dark:text-eggshell "
                            >
                                "Favorite type of cookie"
                            </label>
                            <div class="mt-1">
                                <input
                                    type="text"
                                    name="favorite_cookie_type"
                                    id="favorite_cookie_type"
                                    class="block w-full p-2 max-w-[250px] rounded-md border border-black text-black"
                                    required
                                />
                            </div>
                        </div>
                        <div>
                            <label
                                for="favorite_color"
                                class="block text-sm font-bold text-black dark:text-eggshell"
                            >
                                "Favorite color"
                            </label>
                            <div class="mt-1">
                                <input
                                    type="text"
                                    id="favorite_color"
                                    name="favorite_color"
                                    class="block w-full p-2 max-w-[250px] rounded-md border border-black text-black"
                                    required
                                />
                            </div>
                        </div>
                        <div class="flex items-center">
                            <button class="block max-w-fit mt-1 text-lg py-2 px-4 text-purple dark:text-eggshell rounded-md border border-purple dark:border-eggshell">
                                "Submit"
                            </button>
                            <Show when=favorites.pending()>
                                <div class=" text-black dark:text-eggshell h-4 ml-4">
                                    "Loading..."
                                </div>
                            </Show>
                        </div>
                    </div>
                </div>
            </div>
        </ActionForm>
        <Show when=move || value.with(Option::is_some)>
            <div class="text-center text-black dark:text-eggshell">{value}</div>
        </Show>
    }
}
