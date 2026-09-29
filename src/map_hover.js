"use strict";

(() => {
  const SVG_NS = "http://www.w3.org/2000/svg";
  const HOVER_RADIUS_PX = 24;
  const HIGHLIGHT_RADIUS = 11;
  const TOOLTIP_GAP = 12;

  function htmlElement(name, className, text) {
    const element = document.createElement(name);
    element.className = className;
    if (text !== undefined) {
      element.textContent = text;
    }
    return element;
  }

  function setUp(figure) {
    const svg = figure.querySelector(".map-plot svg");
    const tooltip = figure.querySelector(".map-tooltip");
    const data = JSON.parse(figure.querySelector(".map-data").textContent);
    if (!svg || data.vehicles.length === 0) {
      return;
    }
    const highlight = document.createElementNS(SVG_NS, "circle");
    highlight.setAttribute("class", "map-highlight");
    highlight.setAttribute("r", HIGHLIGHT_RADIUS);
    highlight.setAttribute("display", "none");
    svg.append(highlight);

    function scale() {
      return svg.getBoundingClientRect().width / data.width;
    }

    function nearest(event) {
      const box = svg.getBoundingClientRect();
      const ratio = scale();
      const x = (event.clientX - box.left) / ratio;
      const y = (event.clientY - box.top) / ratio;
      let best = null;
      let bestDistance = HOVER_RADIUS_PX / ratio;
      for (const vehicle of data.vehicles) {
        const distance = Math.hypot(vehicle.x - x, vehicle.y - y);
        if (distance <= bestDistance) {
          best = vehicle;
          bestDistance = distance;
        }
      }
      return best;
    }

    function place(vehicle) {
      const svgBox = svg.getBoundingClientRect();
      const figureBox = figure.getBoundingClientRect();
      const ratio = scale();
      const x = svgBox.left - figureBox.left + vehicle.x * ratio;
      const y = svgBox.top - figureBox.top + vehicle.y * ratio;
      let left = x + TOOLTIP_GAP;
      if (left + tooltip.offsetWidth > figure.clientWidth) {
        left = x - TOOLTIP_GAP - tooltip.offsetWidth;
      }
      tooltip.style.left = `${Math.max(left, 0)}px`;
      tooltip.style.top = `${y + TOOLTIP_GAP}px`;
    }

    function show(vehicle) {
      highlight.setAttribute("cx", vehicle.x);
      highlight.setAttribute("cy", vehicle.y);
      highlight.removeAttribute("display");
      const key = htmlElement("span", "chart-tooltip-key");
      key.style.background = vehicle.color;
      const row = htmlElement("div", "chart-tooltip-row");
      row.append(
        key,
        htmlElement("span", "chart-tooltip-name", vehicle.name),
        htmlElement("span", "chart-tooltip-value", vehicle.speed),
      );
      tooltip.replaceChildren(row);
      tooltip.hidden = false;
      place(vehicle);
    }

    function hide() {
      highlight.setAttribute("display", "none");
      tooltip.hidden = true;
    }

    function update(event) {
      const vehicle = nearest(event);
      if (vehicle) {
        show(vehicle);
      } else {
        hide();
      }
    }

    svg.addEventListener("pointermove", update);
    svg.addEventListener("pointerdown", update);
    svg.addEventListener("pointerleave", hide);
  }

  document.querySelectorAll("figure.map").forEach(setUp);
})();
