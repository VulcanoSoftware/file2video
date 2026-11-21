// Command builder tab: mirrors the Rust encoder's exact frame/RS bookkeeping
// (src/main.rs encode_stealth, src/format.rs Layout::compact, src/stealth.rs
// Geometry::build) and the CLI's incompatibility rules. Everything runs
// client-side: uploaded files are only read with FileReader/video metadata and
// never leave the browser.
(() => {
  const bEl = (id) => document.getElementById(id);
  const bVal = (id) => bEl(id).value.trim();
  const bNum = (id) => { const v = bVal(id); return v === "" ? null : Number(v); };

  function toast(title, message) {
    const box = document.createElement("div");
    box.className = "toast";
    const strong = document.createElement("b");
    strong.textContent = title;
    box.appendChild(strong);
    box.appendChild(document.createTextNode(message));
    box.title = "Click to dismiss";
    box.addEventListener("click", () => box.remove());
    bEl("toasts").appendChild(box);
    setTimeout(() => box.remove(), 7000);
  }

  function fmtBytes(n) {
    if (!isFinite(n)) return "?";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let v = n, i = 0;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return (i === 0 ? String(v) : v.toFixed(v >= 100 ? 0 : 2)) + " " + units[i];
  }

  function fmtTime(secs) {
    if (!isFinite(secs) || secs <= 0 || secs > 86400 * 400) return "?";
    const m = Math.floor(secs / 60), s = Math.round(secs % 60);
    if (m >= 60) return Math.floor(m / 60) + "h " + (m % 60) + "m";
    return (m ? m + "m " : "") + s + "s";
  }

  const bStealth = () => bEl("b-stealth-toggle").value === "on";
  let sourceFile = null;
  let bgMeta = null; // {width, height, duration} read from the uploaded video

  function clampField(id, min, max) {
    const v = bNum(id);
    if (v === null) return;
    const fixed = Math.min(max, Math.max(min, Math.round(v)));
    if (fixed !== v) bEl(id).value = String(fixed);
  }

  // The CLI refuses (or silently ignores) these combinations; the builder
  // refuses them too, with a popup, instead of handing you a broken command.
  function resolveConflicts(changedId) {
    if (bStealth() && bEl("b-chroma").checked) {
      bEl("b-chroma").checked = false;
      toast("Incompatible options",
        "--chroma cannot be combined with --stealth: the chroma planes carry the background colours in stealth encoding. I turned --chroma back off for you.");
    }
    if (bStealth() && bVal("b-optimise") !== "") {
      bEl("b-optimise").value = "";
      toast("Incompatible options",
        "--optimise cannot be combined with --stealth: the calibration profiles only cover the cell format. Pick --gpu and --intra yourself instead; the advice panel proposes them from your priorities.");
    }
    if (bEl("b-no-password").checked && (bVal("b-password") || bVal("b-password-file"))) {
      const message = "--no-password cannot be combined with a password: either encrypt with --password/--password-file or store the file unprotected, never both. I reset the option that caused it.";
      if (changedId === "b-no-password" || !changedId) {
        bEl("b-no-password").checked = false;
        toast("Incompatible options", message);
      } else if (changedId === "b-password") {
        bEl("b-password").value = "";
        toast("Incompatible options", message);
      } else if (changedId === "b-password-file") {
        bEl("b-password-file").value = "";
        toast("Incompatible options", message);
      }
    }
    // --width and --height only make sense together, and odd dimensions break yuv420p.
    if (bNum("b-width") !== null && bNum("b-height") !== null) {
      ["b-width", "b-height"].forEach((id) => {
        const v = bNum(id);
        if (v % 2 !== 0) bEl(id).value = String(v - 1);
      });
    }
  }

  function applyAvailability() {
    const stealth = bStealth();
    bEl("b-chroma").disabled = stealth;
    bEl("b-optimise").disabled = stealth;
    bEl("b-block-size").disabled = stealth;
    ["b-stealth", "b-stealth-block", "b-stealth-intensity", "b-stealth-dense", "b-stealth-no-audio", "b-bg-file"]
      .forEach((id) => { bEl(id).disabled = !stealth; });
    const noPassword = bEl("b-no-password").checked;
    bEl("b-password").disabled = noPassword;
    bEl("b-password-file").disabled = noPassword;
  }

  // ---- Advice from the priority sliders --------------------------------------
  function buildAdvice() {
    const size = Number(bEl("b-priority-size").value);
    const speed = Number(bEl("b-priority-speed").value);
    const robust = Number(bEl("b-priority-robust").value);
    const stealth = bStealth();
    const args = [];
    const why = [];
    const pick = {};
    if (stealth) {
      pick.block = robust >= 8 ? 6 : 4;
      pick.intensity = robust >= 8 ? 50 : robust >= 6 ? 40 : 25;
      pick.dense = speed >= 8 && robust <= 5;
      if (pick.dense) { pick.intensity = Math.max(pick.intensity, 40); if (robust <= 3) pick.block = 2; }
      pick.ecc = robust >= 8 ? 96 : 64;
      const spacing = (2 * pick.intensity) / ((1 << (pick.dense ? 4 : 2)) - 1);
      const safe = Math.floor(17 + 3.5 * Math.log2(spacing / 3.33));
      pick.crf = Math.max(8, Math.min(safe, size >= 8 ? 17 : 15));
      args.push("--stealth <background.mp4>", "--stealth-block " + pick.block,
        "--stealth-intensity " + pick.intensity, "--ecc-symbols " + pick.ecc, "--crf " + pick.crf);
      if (pick.dense) args.push("--stealth-dense");
      pick.intra = robust >= 7;
      if (pick.intra) args.push("--intra");
      why.push(robust >= 8
        ? "Maximum robustness: block 6 and intensity 50 survive aggressive re-encoding (YouTube), and 96 parity symbols add margin \u2014 at a clear size and speed cost."
        : robust >= 6
          ? "Elevated robustness: intensity 40 keeps the two luma layers readable on aggressive platforms while block 4 keeps throughput decent."
          : "Default balance: intensity 25 with block 4 keeps the layer subtle, the video small and the encode fast.");
      why.push("CRF " + pick.crf + " stays inside the measured restore boundary for a " +
        spacing.toFixed(1) + "-level spacing; raising CRF shrinks the MP4 but past that boundary the restore fails.");
      why.push("Expect a stealth MP4 of roughly 4\u20137\u00d7 the payload regardless \u2014 the hidden layer is near-random noise, and that entropy has to fit in the video.");
    } else {
      if (size >= 8) {
        pick.compress = "high";
        why.push("Size first: --compress high picks preset veryslow with the tightest (16) parity budget, so the MP4 shrinks the most.");
      } else if (speed >= 8) {
        pick.compress = "light";
        why.push("Speed first: --compress light keeps CRF 8 but uses the fast preset with an extra (64) recovery margin, so encode and decode both stay cheap.");
      } else if (size >= 6) {
        pick.compress = "medium";
        why.push("Balanced: --compress medium is the default balance of speed and size.");
      }
      pick.intra = robust >= 7;
      if (pick.intra) {
        args.push("--intra");
        why.push("Robustness: --intra makes every frame a keyframe, which restores far more reliably and decodes faster.");
      }
      pick.chroma = speed >= 8 && robust <= 4;
      if (pick.chroma) {
        why.push("Speed: --chroma squeezes ~33% more bytes into every frame for the same MP4 size; run one round trip on your GPU encoder to verify it before trusting production files.");
      }
    }
    return { args, why, pick };
  }

  function renderAdvice() {
    const a = buildAdvice();
    const chips = bEl("b-advice-chips");
    chips.innerHTML = "";
    const list = a.args.length ? a.args : ["(no extra flags needed \u2014 the defaults already match)"];
    list.forEach((arg) => {
      const chip = document.createElement("span");
      chip.className = "chip";
      chip.textContent = arg;
      chips.appendChild(chip);
    });
    bEl("b-advice-text").textContent = a.why.join(" ");
    return a;
  }

  bEl("b-advice-apply").addEventListener("click", () => {
    const a = buildAdvice();
    const p = a.pick;
    if (bStealth()) {
      if (p.block) bEl("b-stealth-block").value = String(p.block);
      if (p.intensity) bEl("b-stealth-intensity").value = String(p.intensity);
      bEl("b-stealth-dense").checked = !!p.dense;
      if (p.ecc) bEl("b-ecc").value = String(p.ecc);
      if (p.crf) bEl("b-crf").value = String(p.crf);
      bEl("b-intra").checked = !!p.intra;
    } else {
      if (p.compress) bEl("b-compress").value = p.compress;
      bEl("b-intra").checked = !!p.intra;
      bEl("b-chroma").checked = !!p.chroma;
    }
    resolveConflicts(null);
    render2();
    toast("Advice applied",
      "The recommended flags are now in the form; tweak anything and the command updates live.");
  });

  // ---- Client-side estimator ---------------------------------------------------
  function layoutFor(encrypted, parity) {
    const words = Math.ceil((encrypted + 30) / (255 - parity));
    return { words, protectedLen: 30 + words * 255 };
  }

  function clear(node) { node.innerHTML = ""; }

  function renderEstimate() {
    const dl = bEl("b-estimate");
    const warnBox = bEl("b-warnings");
    clear(dl);
    clear(warnBox);
    warnBox.classList.remove("visible");

    const addRow = (term, value, warnValue) => {
      const dt = document.createElement("dt");
      dt.textContent = term;
      const dd = document.createElement("dd");
      dd.textContent = value;
      if (warnValue) dd.className = "warn-value";
      dl.appendChild(dt);
      dl.appendChild(dd);
    };
    const addWarn = (bad, text) => {
      warnBox.classList.add("visible");
      const p = document.createElement("p");
      if (bad) p.className = "bad";
      p.textContent = (bad ? "\u26a0 " : "\u2139 ") + text;
      warnBox.appendChild(p);
    };

    const stealth = bStealth();
    const size = sourceFile ? sourceFile.size : (bNum("b-source-size") || 0);
    const name = (sourceFile ? sourceFile.name : bVal("b-source-name")) || "payload.bin";
    if (!size) {
      addWarn(false, "Pick a source file (or type its size) to compute the numbers.");
      return;
    }
    if (stealth && !bgMeta) {
      addWarn(false, "No background video measured in the browser \u2014 guessing 1920\u00d71080 as the geometry. Upload the actual video for exact frame counts (its pixels never leave your machine).");
    }
    if (!stealth && (bNum("b-width") === null) !== (bNum("b-height") === null)) {
      addWarn(false, "--width only takes effect together with --height; fill in both (or neither).");
    }

    const nameBytes = new TextEncoder().encode(name).length;
    const plain = size + 22 + nameBytes;
    const encrypted = plain + 128; // the crypto (or integrity) header precedes the payload
    const parity = bNum("b-ecc") ?? (stealth
      ? 64
      : { light: 64, medium: 32, high: 16 }[bEl("b-compress").value] || 32);
    const { words, protectedLen } = layoutFor(encrypted, parity);
    const fps = bNum("b-fps") ?? 30;
    const noPassword = bEl("b-no-password").checked;
    if (stealth && parity > 64) {
      addWarn(false, "Parity " + parity + " adds ~" + (parity - 64) + "% stream size over the 64 default; lower --ecc-symbols (or --stealth-intensity) if the file size matters.");
    }

    let frames = 0;
    let est = 0;
    let crosses = false;
    if (stealth) {
      const block = bNum("b-stealth-block") ?? 4;
      const intensity = bNum("b-stealth-intensity") ?? 25;
      const sb = bEl("b-stealth-dense").checked ? 4 : 2;
      const bgW = bgMeta ? bgMeta.width : 1920;
      const bgH = bgMeta ? bgMeta.height : 1080;
      const width = bNum("b-width") ?? (bgW - (bgW % block));
      const height = bNum("b-height") ?? (bgH - (bgH % block));
      const columns = Math.floor(width / block);
      const rows = Math.floor(height / block);
      const pairs = Math.floor((columns - 2) / 2);
      const datarows = rows - 4;
      const frameBytes = pairs * datarows * sb / 8;
      const dataBytes = 8 + 448 + protectedLen - 30;
      frames = Math.ceil(dataBytes / Math.max(1, frameBytes));
      const spacing = (2 * intensity) / ((1 << sb) - 1);
      const safeCrf = Math.floor(17 + 3.5 * Math.log2(spacing / 3.33));
      const crf = bNum("b-crf") ?? 18;
      addRow("Data per frame", fmtBytes(frameBytes) + " (" + pairs + " pairs \u00d7 " + datarows + " rows \u00d7 " + sb + " bits)");
      addRow("Level spacing", spacing.toFixed(1) + " luma levels \u2014 restores up to ~CRF " + safeCrf);
      if (crf > safeCrf) {
        addWarn(true, "Restore will probably FAIL: CRF " + crf + " is lossier than the level spacing survives (safe up to about CRF " + safeCrf + ", measured). Use --crf " + safeCrf + ", a higher --stealth-intensity, or drop --stealth-dense.");
      }
      // Measured 720p x264 anchors: crf16/I50/64p \u2248 6.5\u00d7 payload, crf18/I50/96p \u2248 8\u00d7.
      const mult = 6.5 * Math.pow(2, (16 - crf) * 0.05) * (1 + (intensity - 50) * 0.006)
        * (1 + (parity - 64) * 0.01) * (sb === 2 ? 0.97 : 1);
      est = size * mult;
      crosses = est / size > 3;
      addRow("Estimated MP4 size", "\u2248 " + fmtBytes(est * 0.75) + " \u2026 " + fmtBytes(est * 1.33) + "  (\u2248 " + (est / size).toFixed(1) + "\u00d7 the payload)", crosses);
      addRow("Same file without stealth", "\u2248 " + fmtBytes(size * 1.35) + " \u2014 the cell format wins when camouflage is not needed");
      if (pairs <= 0 || rows <= 4 || columns <= 2) {
        addWarn(true, "These dimensions are too small for the stealth anchor border; lower --stealth-block or use the background's own resolution.");
      }
    } else {
      const block = bNum("b-block-size") ?? 8;
      const dw = bNum("b-width") ?? 1920, dh = bNum("b-height") ?? 1080;
      const ceilEven = (n) => n + (n % 2);
      const cellW = ceilEven(Math.max(256, Math.floor(dw / block)));
      const cellH = ceilEven(Math.max(256, Math.floor(dh / block)));
      const cells = cellW * cellH * (bEl("b-chroma").checked ? 1.5 : 1);
      frames = Math.ceil(((448 + protectedLen) * 8) / (cells * 4));
      const presetFac = { ultrafast: 1.9, superfast: 1.5, veryfast: 1.2, faster: 1.05,
        fast: 1.0, medium: 0.95, slow: 0.92, slower: 0.9, veryslow: 0.88 };
      const crf = bNum("b-crf") ?? 8;
      if (crf > 8) {
        addWarn(true, "The cell format caps CRF at 8: above it data cells can be quantised away and the restore fails. Keep --crf at 8 (or use --compress and leave --crf out).");
      }
      est = size * 1.55 * (presetFac[bVal("b-preset")] || 1.05) * Math.pow(2, (8 - crf) * 0.03);
      addRow("Cell area", Math.floor(dw / block) + "\u00d7" + Math.floor(dh / block) + " cells of " + block + "\u00d7" + block + " px (\u2248" + Math.round(cells).toLocaleString() + " effective cells)");
      addRow("Estimated MP4 size", "\u2248 " + fmtBytes(est * 0.75) + " \u2026 " + fmtBytes(est * 1.33) + "  (\u2248 " + (est / size).toFixed(2) + "\u00d7 the payload)");
    }

    addRow("Reed-Solomon stream", fmtBytes(protectedLen) + " (" + words.toLocaleString() + " codewords, " + Math.round((parity / 255) * 100) + "% parity)");
    addRow("Payload + overhead", fmtBytes(size) + " + " + fmtBytes(plain - size) + " metadata + 128 B " + (noPassword ? "integrity header" : "AES-GCM header"));
    if (frames > 0) {
      addRow("Video frames", frames.toLocaleString() + " at " + fps + " fps");
      addRow("Playback duration", fmtTime(frames / fps));
      const rate = size / (frames / fps);
      addRow("Effective data rate", fmtBytes(rate) + "/s of video");
      if (stealth && bgMeta && bgMeta.duration) {
        addRow("Background loops", Math.ceil((frames / fps) / bgMeta.duration) + "\u00d7 (background is " + fmtTime(bgMeta.duration) + ")");
      }
      if (stealth) {
        const audioBytes = (frames / fps) * 24000; // AAC 192 kbit/s
        if (bEl("b-stealth-no-audio").checked) {
          addRow("Background audio", "dropped with --stealth-no-audio (saved \u2248 " + fmtBytes(audioBytes) + ")");
        } else {
          addRow("Background audio", "muxed as AAC 192 kbit/s (\u2248 " + fmtBytes(audioBytes) + ") \u2014 tick --stealth-no-audio to drop it");
        }
      }
      if (stealth) {
        const block = bNum("b-stealth-block") ?? 4;
        addRow("Encode wall-clock guess", "\u2248 " + fmtTime(frames / (275 * 16 / (block * block))) + " on an AMD 760M iGPU at 720p (\u2248275 fps measured at block 4; scale for your GPU)");
      } else {
        addRow("Encode wall-clock guess", "\u2248 " + fmtTime(size / (11.8 * 1024 * 1024)) + " on the calibrated CPU; with a GPU encoder usually faster \u2014 run `file2video benchmark` for real numbers");
      }
    }
    if (!sourceFile && bVal("b-source-name") === "") {
      addWarn(false, "Type the real file name too if you want it in the command; the name itself costs " + (22 + nameBytes) + " bytes of stream.");
    }
  }

  // ---- Command string -----------------------------------------------------------
  function quoteArg(value) {
    return /\s/.test(value) ? '"' + value.replace(/"/g, '\\"') + '"' : value;
  }

  function renderCommand() {
    const parts = ["file2video", "encode"];
    const src = sourceFile ? sourceFile.name : (bVal("b-source-name") || "input.bin");
    let out = bVal("b-output");
    if (!out) {
      const base = src.replace(/\.[^.\/\\]+$/, "");
      out = (base || "output") + ".mp4";
    }
    parts.push(quoteArg(src), quoteArg(out));
    if (bStealth()) {
      parts.push("--stealth", quoteArg(bVal("b-stealth") || "<background.mp4>"));
      if (bNum("b-stealth-block") !== null) parts.push("--stealth-block " + bNum("b-stealth-block"));
      if (bNum("b-stealth-intensity") !== null) parts.push("--stealth-intensity " + bNum("b-stealth-intensity"));
      if (bEl("b-stealth-dense").checked) parts.push("--stealth-dense");
      if (bEl("b-stealth-no-audio").checked) parts.push("--stealth-no-audio");
    } else {
      if (bNum("b-block-size") !== null) parts.push("--block-size " + bNum("b-block-size"));
      if (bEl("b-chroma").checked) parts.push("--chroma");
    }
    if (bNum("b-width") !== null && bNum("b-height") !== null) {
      parts.push("--width " + bNum("b-width"), "--height " + bNum("b-height"));
    }
    if (bNum("b-fps") !== null) parts.push("--fps " + bNum("b-fps"));
    if (bEl("b-intra").checked) parts.push("--intra");
    if (bVal("b-compress")) parts.push("--compress " + bVal("b-compress"));
    if (bNum("b-crf") !== null) parts.push("--crf " + bNum("b-crf"));
    if (bVal("b-preset")) parts.push("--preset " + bVal("b-preset"));
    if (bNum("b-ecc") !== null) parts.push("--ecc-symbols " + bNum("b-ecc"));
    if (bNum("b-pbkdf2") !== null) parts.push("--pbkdf2-iterations " + bNum("b-pbkdf2"));
    if (bVal("b-gpu")) parts.push("--gpu " + bVal("b-gpu"));
    if (bNum("b-threads") !== null) parts.push("--threads " + bNum("b-threads"));
    if (bVal("b-optimise") && !bStealth()) parts.push("--optimise " + bVal("b-optimise"));
    if (bEl("b-no-password").checked) {
      parts.push("--no-password");
    } else if (bVal("b-password-file")) {
      parts.push("--password-file", quoteArg(bVal("b-password-file")));
    } else if (bVal("b-password")) {
      parts.push("--password", quoteArg(bVal("b-password")));
    }
    const decode = "file2video decode " + quoteArg(out) + " " + quoteArg(src) +
      (bEl("b-no-password").checked ? "" : "   # asks for the password");
    bEl("b-command").value = parts.join(" ") + "\n\n# later, on any machine (decoding is blind \u2014 no flags needed):\n" + decode;
  }

  function render2() {
    resolveConflicts(null);
    applyAvailability();
    renderAdvice();
    renderEstimate();
    renderCommand();
  }

  const B_IDS = ["b-priority-size", "b-priority-speed", "b-priority-robust", "b-stealth-toggle",
    "b-source-size", "b-source-name", "b-output", "b-width", "b-height", "b-block-size", "b-fps",
    "b-chroma", "b-intra", "b-compress", "b-crf", "b-preset", "b-ecc", "b-pbkdf2",
    "b-no-password", "b-password", "b-password-file", "b-stealth", "b-stealth-block",
    "b-stealth-no-audio",
    "b-stealth-intensity", "b-stealth-dense", "b-gpu", "b-threads", "b-optimise"];
  const CLAMPS = { "b-fps": [1, 60], "b-crf": [0, 51], "b-ecc": [1, 127], "b-pbkdf2": [1, 2147483647],
    "b-threads": [0, 256], "b-stealth-intensity": [5, 50], "b-source-size": [1, 9007199254740991] };

  B_IDS.forEach((id) => {
    const node = bEl(id);
    if (!node) return;
    const onChange = () => {
      if (CLAMPS[id]) clampField(id, CLAMPS[id][0], CLAMPS[id][1]);
      resolveConflicts(id);
      applyAvailability();
      renderAdvice();
      renderEstimate();
      renderCommand();
    };
    node.addEventListener("input", onChange);
    node.addEventListener("change", onChange);
  });

  ["b-priority-size", "b-priority-speed", "b-priority-robust"].forEach((id) => {
    bEl(id).addEventListener("input", () => { bEl(id + "-out").textContent = bEl(id).value; });
  });

  // source picker: read size + name, nothing else ever leaves the browser
  bEl("b-source-file").addEventListener("change", () => {
    const file = bEl("b-source-file").files[0];
    if (!file) { sourceFile = null; return; }
    sourceFile = file;
    bEl("b-source-size").value = String(file.size);
    bEl("b-source-name").value = file.name;
    bEl("b-source-info").textContent = file.name + " \u2014 " + fmtBytes(file.size);
    render2();
  });

  // background picker: video metadata only (resolution, duration)
  bEl("b-bg-file").addEventListener("change", () => {
    const file = bEl("b-bg-file").files[0];
    if (!file) {
      bgMeta = null;
      bEl("b-bg-info").textContent = "No background chosen \u2014 estimates then assume 1920\u00d71080.";
      render2();
      return;
    }
    const url = URL.createObjectURL(file);
    const probe = document.createElement("video");
    probe.preload = "metadata";
    probe.onloadedmetadata = () => {
      bgMeta = { width: probe.videoWidth || 0, height: probe.videoHeight || 0, duration: probe.duration || 0 };
      bEl("b-bg-info").textContent = file.name + " \u2014 " + bgMeta.width + "\u00d7" + bgMeta.height + ", " + fmtTime(bgMeta.duration);
      if (!bVal("b-stealth")) bEl("b-stealth").value = file.name;
      render2();
      URL.revokeObjectURL(url);
    };
    probe.onerror = () => {
      bgMeta = null;
      bEl("b-bg-info").textContent = file.name + " \u2014 the browser could not read its resolution; assuming 1920\u00d71080.";
      if (!bVal("b-stealth")) bEl("b-stealth").value = file.name;
      render2();
      URL.revokeObjectURL(url);
    };
    probe.src = url;
  });

  bEl("b-copy").addEventListener("click", async (event) => {
    const button = event.target;
    try { await navigator.clipboard.writeText(bEl("b-command").value); }
    catch { bEl("b-command").select(); document.execCommand("copy"); }
    button.textContent = "Copied!";
    button.classList.add("copied");
    setTimeout(() => { button.textContent = "Copy to clipboard"; button.classList.remove("copied"); }, 1500);
  });

  // Hand the shared values to the config generator (a config file stores
  // defaults; the command flags built here always win over it).
  bEl("b-to-config").addEventListener("click", () => {
    const share = {
      width: bVal("b-width"), height: bVal("b-height"), block_size: bVal("b-block-size"),
      fps: bVal("b-fps"), chroma: bEl("b-chroma").checked, intra: bEl("b-intra").checked,
      compress: bVal("b-compress"), crf: bVal("b-crf"), preset: bVal("b-preset"),
      ecc_symbols: bVal("b-ecc"), pbkdf2_iterations: bVal("b-pbkdf2"), threads: bVal("b-threads"),
      gpu: bVal("b-gpu"), optimise: bVal("b-optimise"),
      stealth_intensity: bVal("b-stealth-intensity"), stealth_block: bVal("b-stealth-block"),
      stealth_dense: bEl("b-stealth-dense").checked,
      stealth_no_audio: bEl("b-stealth-no-audio").checked,
    };
    FIELDS.forEach((field) => {
      const node = el(field);
      if (!(field in share)) return;
      if (node.type === "checkbox") node.checked = !!share[field];
      else node.value = share[field];
    });
    render();
    switchTab("config");
    toast("Config generator filled",
      "The shared values from the command builder are now in the config form. Note: a config file stores defaults \u2014 flags from the builder's command always win.");
  });

  function switchTab(name) {
    bEl("tab-config").hidden = name !== "config";
    bEl("tab-builder").hidden = name !== "builder";
    bEl("tabbtn-config").classList.toggle("active", name === "config");
    bEl("tabbtn-builder").classList.toggle("active", name === "builder");
  }
  bEl("tabbtn-config").addEventListener("click", () => switchTab("config"));
  bEl("tabbtn-builder").addEventListener("click", () => switchTab("builder"));

  render2();
})();
