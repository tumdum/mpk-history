"use strict";

(() => {
  const SVG_NS = "http://www.w3.org/2000/svg";
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
    const data = JSON.parse(figure.querySelector(".speed-map-data").textContent);
    if (!svg || data.cells.length === 0) {
      return;
    }
    const highlight = document.createElementNS(SVG_NS, "polygon");
    highlight.setAttribute("class", "speed-highlight");
    highlight.setAttribute("display", "none");
    svg.append(highlight);

    function heading() {
      const row = htmlElement("div", "speed-tooltip-row");
      row.append(
        htmlElement("span", ""),
        htmlElement("span", ""),
        ...data.columns.map((column) => htmlElement("span", "speed-tooltip-heading", column)),
      );
      return row;
    }

    function subjectRow(subject, speeds) {
      const key = htmlElement("span", "chart-tooltip-key");
      key.style.background = subject.color;
      const row = htmlElement("div", "speed-tooltip-row");
      row.append(
        key,
        htmlElement("span", "chart-tooltip-name", subject.name),
        ...speeds.map((speed) => htmlElement("span", "chart-tooltip-value", speed)),
      );
      return row;
    }

    function place(event) {
      const box = figure.getBoundingClientRect();
      const x = event.clientX - box.left;
      const y = event.clientY - box.top;
      let left = x + TOOLTIP_GAP;
      if (left + tooltip.offsetWidth > figure.clientWidth) {
        left = x - TOOLTIP_GAP - tooltip.offsetWidth;
      }
      tooltip.style.left = `${Math.max(left, 0)}px`;
      tooltip.style.top = `${y + TOOLTIP_GAP}px`;
    }

    function show(cell, event) {
      highlight.setAttribute("points", cell.getAttribute("points"));
      highlight.removeAttribute("display");
      const speeds = data.cells[Number(cell.dataset.cell)];
      const rows = [htmlElement("div", "chart-tooltip-time", data.unit), heading()];
      data.subjects.forEach((subject, index) => {
        if (speeds[index]) {
          rows.push(subjectRow(subject, speeds[index]));
        }
      });
      tooltip.replaceChildren(...rows);
      tooltip.hidden = false;
      place(event);
    }

    function hide() {
      highlight.setAttribute("display", "none");
      tooltip.hidden = true;
    }

    function update(event) {
      const cell = event.target.closest("[data-cell]");
      if (cell && svg.contains(cell)) {
        show(cell, event);
      } else {
        hide();
      }
    }

    svg.addEventListener("pointermove", update);
    svg.addEventListener("pointerdown", update);
    svg.addEventListener("pointerleave", hide);
  }

  document.querySelectorAll("figure.speed-map").forEach(setUp);
})();
