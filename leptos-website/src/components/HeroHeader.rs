#![allow(dead_code)]

use crate::components::SecondaryButton;
use crate::components::SphereLogo::*;
use crate::icons::DigitalOceanIcon;
use leptos::*;
use leptos_meta::Style;
use leptos_router::A;

#[component]
pub fn HeroHeader() -> impl IntoView {
    view! {
        <HamburgerProvider>
            <Style>{include_str!("./HeroHeader.css")}</Style>
            <div class="max-w-[1920px] mx-auto relative">
                <div class="relative bg-no-repeat bg-center bg-[length:100%_100%] w-[calc(100%+25px)]  h-full top-0 4xl:top-[-10px] left-[-15px] 4xl:left-0 hero-header drop-shadow-[10px_10px_0px_#190E3825] bg-background min-h-[50vh] text-white">
                    <div class="pl-6 pr-4 md:pl-8 md:pr-6 lg:pl-12 lg:pr-8 pt-2 md:pt-4 lg:pt-8 text-white  w-full ">
                        <div class="flex w-full items-center justify-between max-w-4xl mx-auto">
                            <A href="/">
                                <img
                                    class="block w-[90px] h-[38px] sm:w-28 sm:h-auto"
                                    src="/images/videocall_logo.svg"
                                    alt="Home"
                                />
                            </A>
                            <ShowWhenOpen is=false>
                                <div class="hidden lg:block">
                                    <div class="flex gap-1 xl:gap-4 justify-center items-center">
                                        <a
                                            href="#solutions"
                                            class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                        >
                                            "Solutions"
                                        </a>
                                        <a
                                            href="#developers"
                                            class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                        >
                                            "Developers"
                                        </a>
                                        <a
                                            href="#company"
                                            class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                        >
                                            "Company"
                                        </a>
                                        <a
                                            href="#customers"
                                            class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                        >
                                            "Customers"
                                        </a>
                                        <a
                                            href="#pricing"
                                            class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                        >
                                            "Pricing"
                                        </a>
                                    </div>
                                </div>
                            </ShowWhenOpen>
                            <div>
                                <div class="flex gap-4 md:gap-6 justify-center items-center">
                                    <a href="https://discord.gg/XRdt6WfZyf">
                                        <img
                                            class="block h-6 w-6"
                                            src="/images/discord_logo.svg"
                                            alt="Discord"
                                        />
                                    </a>
                                    <a href="https://github.com/security-union/videocall-rs">
                                        <img
                                            class="block h-6 w-6"
                                            src="/images/github_logo.svg"
                                            alt="GitHub"
                                        />
                                    </a>
                                    <a href="https://m.do.co/c/6de4e19c5193" class="block h-8 w-24">
                                        <DigitalOceanIcon />
                                    </a>
                                    <DarkModeToggle/>
                                </div>
                            </div>
                        </div>
                        <ShowWhenOpen is=true>
                            <div class="max-w-4xl mx-auto mt-8 pb-16 lg:hidden">
                                <div class="flex flex-col gap-2">
                                    <a
                                        href="#solutions"
                                        class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                    >
                                        "Solutions"
                                    </a>
                                    <a
                                        href="#developers"
                                        class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                    >
                                        "Developers"
                                    </a>
                                    <a
                                        href="#company"
                                        class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                    >
                                        "Company"
                                    </a>
                                    <a
                                        href="#customers"
                                        class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                    >
                                        "Customers"
                                    </a>
                                    <a
                                        href="#pricing"
                                        class="font-medium transition-all text-gray-300 hover:text-white hover:bg-primary/10 px-4 py-2 rounded-md"
                                    >
                                        "Pricing"
                                    </a>
                                </div>
                            </div>
                        </ShowWhenOpen>
                        <ShowWhenOpen is=false>
                            <div class="max-w-4xl mx-auto relative">
                                <div class="flex gap-12 justify-start xl:justify-between items-center pt-12 pb-24 md:py-24">
                                    <div class="md:mt-[-60px] lg:mt-[-60px]">
                                        <h1 class="font-bold text-4xl lg:text-5xl tracking-tight text-white">
                                            "Ultra-low latency videocalls"
                                        </h1>
                                        <p class="mt-2 text-gray-300 max-w-[40ch]">
                                            "Always open-source, always awesome, powered by Rust"
                                        </p>
                                        <SecondaryButton
                                            title="Watch How It Works"
                                            href=Some("https://www.youtube.com/watch?v=kZ9isFw1TQ8&list=PLxM2CWwQlzBtmhdaK2IeuQvpepKC6BYLOs".to_string())
                                            class="mt-4"
                                        />
                                    </div>
                                    <div class="w-[40%]"></div>
                                    <div class="max-w-[320px] md:max-w-[360px] lg:max-w-[400px] hidden md:block w-full h-full absolute right-0 top-4 aspect-square">
                                        <SphereLogo/>
                                    </div>
                                </div>
                            </div>
                        </ShowWhenOpen>
                    </div>
                </div>
            </div>
        </HamburgerProvider>
    }
}

#[island]
fn HamburgerProvider(children: Children) -> impl IntoView {
    provide_context(RwSignal::new(false));

    children()
}

#[island]
fn DarkModeToggle() -> impl IntoView {
    let (hamburger_menu_open, set_hamburger_menu_open) = expect_context::<RwSignal<bool>>().split();

    view! {
        <button on:click=move |_| set_hamburger_menu_open.update(|n| *n = !*n)>
            <img
                class=" h-6 w-6 block lg:hidden"
                src=move || match hamburger_menu_open() {
                    true => "/images/x_close.svg",
                    false => "/images/mobile_menu.svg",
                }
                alt="Toggle Menu"
            />
        </button>
    }
}

#[island]
fn ShowWhenOpen(is: bool, children: Children) -> impl IntoView {
    let hamburger_menu_open = expect_context::<RwSignal<bool>>().read_only();

    view! {
        <div style:display=move || if hamburger_menu_open() == is {
            "contents"
        } else {
            "none"
        }>
            {children()}
        </div>
    }
}
