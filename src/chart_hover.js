"use strict";

(() => {
  const SVG_NS = "http://www.w3.org/2000/svg";
  const DOT_RADIUS = 4;
  const TOOLTIP_GAP = 12;
  const STEP_KEYS = { ArrowLeft: -1, ArrowRight: 1 };

  function svgElement(name, attributes) {
    const element = document.createElementNS(SVG_NS, name);
    for (const [key, value] of Object.entries(attributes)) {
      element.setAttribute(key, value);
    }
    return element;
  }

  function htmlElement(name, className, text) {
    const element = document.createElement(name);
    element.className = className;
    if (text !== undefined) {
      element.textContent = text;
    }
    return element;
  }

  function setUp(figure) {
    const svg = figure.querySelector(".chart-plot svg");
    const tooltip = figure.querySelector(".chart-tooltip");
    const data = JSON.parse(figure.querySelector(".chart-data").textContent);
    if (!svg || data.steps.length === 0) {
      return;
    }
    const caption = figure.querySelector("figcaption");
    svg.setAttribute("tabindex", "0");
    svg.setAttribute("role", "img");
    svg.setAttribute("aria-label", caption ? caption.textContent : "");

    const overlay = svgElement("g", { class: "chart-hover", display: "none" });
    const crosshair = svgElement("line", {
      class: "chart-crosshair",
      y1: data.plot.top,
      y2: data.plot.bottom,
    });
    overlay.append(crosshair);
    const dots = data.series.map((series) => {
      const dot = svgElement("circle", { class: "chart-dot", r: DOT_RADIUS, fill: series.color });
      overlay.append(dot);
      return dot;
    });
    svg.append(overlay);

    let current = null;

    function fillTooltip(step) {
      const rows = data.series.map((series, index) => {
        const row = htmlElement("div", "chart-tooltip-row");
        const key = htmlElement("span", "chart-tooltip-key");
        key.style.background = series.color;
        row.append(
          key,
          htmlElement("span", "chart-tooltip-name", series.name),
          htmlElement("span", "chart-tooltip-value", step.values[index].text),
        );
        return row;
      });
      tooltip.replaceChildren(htmlElement("div", "chart-tooltip-time", step.label), ...rows);
    }

    function placeTooltip(step) {
      const svgBox = svg.getBoundingClientRect();
      const figureBox = figure.getBoundingClientRect();
      const scale = svgBox.width / data.width;
      const x = svgBox.left - figureBox.left + step.x * scale;
      let left = x + TOOLTIP_GAP;
      if (left + tooltip.offsetWidth > figure.clientWidth) {
        left = x - TOOLTIP_GAP - tooltip.offsetWidth;
      }
      tooltip.style.left = `${Math.max(left, 0)}px`;
      tooltip.style.top = `${svgBox.top - figureBox.top + data.plot.top * scale}px`;
    }

    function show(index) {
      current = index;
      const step = data.steps[index];
      crosshair.setAttribute("x1", step.x);
      crosshair.setAttribute("x2", step.x);
      step.values.forEach((value, series) => {
        const dot = dots[series];
        if (value.y === null) {
          dot.setAttribute("display", "none");
          return;
        }
        dot.removeAttribute("display");
        dot.setAttribute("cx", step.x);
        dot.setAttribute("cy", value.y);
      });
      overlay.removeAttribute("display");
      fillTooltip(step);
      tooltip.hidden = false;
      placeTooltip(step);
    }

    function hide() {
      current = null;
      overlay.setAttribute("display", "none");
      tooltip.hidden = true;
    }

    function nearestStep(clientX) {
      const box = svg.getBoundingClientRect();
      const x = ((clientX - box.left) * data.width) / box.width;
      if (x < data.plot.left || x > data.plot.right) {
        return null;
      }
      let nearest = 0;
      data.steps.forEach((step, index) => {
        if (Math.abs(step.x - x) < Math.abs(data.steps[nearest].x - x)) {
          nearest = index;
        }
      });
      return nearest;
    }

    svg.addEventListener("pointermove", (event) => {
      const index = nearestStep(event.clientX);
      if (index === null) {
        hide();
        return;
      }
      if (index !== current) {
        show(index);
      }
    });
    svg.addEventListener("pointerleave", hide);
    svg.addEventListener("blur", hide);
    svg.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        hide();
        return;
      }
      const delta = STEP_KEYS[event.key];
      if (delta === undefined) {
        return;
      }
      event.preventDefault();
      const start = current === null ? (delta > 0 ? -1 : data.steps.length) : current;
      show(Math.min(Math.max(start + delta, 0), data.steps.length - 1));
    });
  }

  document.querySelectorAll("figure.chart").forEach(setUp);
})();
