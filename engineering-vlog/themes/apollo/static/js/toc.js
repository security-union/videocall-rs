document.addEventListener("DOMContentLoaded", () => {
  let observer = new IntersectionObserver(handler, {
    threshold: [0],
  });
  let paragraphs = [...document.querySelectorAll("section > *")];
  let submenu = [...document.querySelectorAll(".toc a")];

  function previousHeaderId(e) {
    for (; e && !e.matches("h1, h2, h3, h4"); ) e = e.previousElementSibling;
    return e?.id;
  }
  let paragraphMenuMap = paragraphs.reduce((e, t) => {
    let n = previousHeaderId(t);
    if (((t.previousHeader = n), n)) {
      let t = submenu.find((e) => decodeURIComponent(e.hash) === "#" + n);
      e[n] = t;
    }
    return e;
  }, {});

  paragraphs.forEach((e) => observer.observe(e));
  let selection;
  function handler(e) {
    selection = (selection || e).map(
      (t) => e.find((e) => e.target === t.target) || t,
    );
    for (s of selection)
      s.isIntersecting ||
        paragraphMenuMap[
          s.target.previousHeader
        ]?.parentElement.classList.remove("selected", "parent");
    for (s of selection)
      if (s.isIntersecting) {
        let e = paragraphMenuMap[s.target.previousHeader]?.closest("li");
        if ((e?.classList.add("selected"), e === void 0)) continue;
        let t = e.firstChild;
        for (
          console.log(e, t),
            t.scrollIntoView({
              block: "nearest",
              inline: "nearest",
            });
          e;

        )
          e?.classList.add("parent"), (e = e.parentElement.closest("li"));
      }
  }
});
