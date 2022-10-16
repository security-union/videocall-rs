use yew::prelude::*;

pub struct Attendant;

#[derive(Properties, PartialEq)]
pub struct AttendantProps {
    #[prop_or_default]
    pub name: String,
}

impl Component for Attendant {
    type Message = ();
    type Properties = AttendantProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        html! {
            <>
                <h1>{ctx.props().name.clone()}</h1>
            </>
        }
    }
}
