import { useEffect, useRef, useState, type ReactNode } from "react";
import { Box, useBoxMetrics, useInput } from "ink";
import { mouse, type MouseEvent } from "../mouse.ts";
import type { Viewport } from "../selection.ts";

const WHEEL_ROWS = 3;
/**
 * One glide step every 20 ms (50/s). Just above Ink's frame interval (60/s =
 * 16.7 ms) so every step gets its own frame: at 16 ms the two timers beat
 * against each other and frames came 4 ms, then 50 ms apart (visible stutter).
 */
const GLIDE_MS = 20;

interface Props {
  children: ReactNode;
  /** Change this to jump back to the bottom (e.g. when the user sends a message). */
  followKey?: number;
  isActive?: boolean;
  /** Told where the viewport is and how far it's scrolled on every render, plus how to scroll it (used by mouse selection). */
  onViewport?: (viewport: Viewport, scrollBy: (rows: number) => void) => void;
  /** Kept mounted but not shown (and not scrolled by keys or the wheel): it keeps its scroll position for when it's back. */
  hidden?: boolean;
  /** Start at the top rather than following the bottom (a list read from its start, like /undo's files). */
  fromTop?: boolean;
}

// The alternate screen has no terminal scrollback, so Marv scrolls its own
// transcript. The viewport fills whatever height its parent gives it and
// clips the content, which is shifted up by `contentOffsetY` rows.
//
// It follows the bottom (like a chat) until you press PgUp or scroll up. Then
// it stays where you scrolled, even while a reply streams in, until you scroll
// back down or `followKey` changes.
export function ScrollView({ children, followKey = 0, isActive: active = true, onViewport, hidden = false, fromTop = false }: Props) {
  const isActive = active && !hidden;
  const viewportRef = useRef(null);
  const contentRef = useRef(null);
  const measuredViewport = useBoxMetrics(viewportRef);
  const measuredContent = useBoxMetrics(contentRef);
  // A hidden box measures zero, and so does the first frame after it's shown again (the metrics catch up after
  // it's drawn): clamping to those would draw it scrolled to the top, and a scroll then would lose the position.
  // So while hidden, and until it's measured again, keep the last real measurements.
  const lastMeasured = useRef({ viewport: measuredViewport, content: measuredContent });
  const measuredNow = !hidden && measuredViewport.clientHeight > 0;
  if (measuredNow) lastMeasured.current = { viewport: measuredViewport, content: measuredContent };
  const { viewport, content } = measuredNow ? { viewport: measuredViewport, content: measuredContent } : lastMeasured.current;

  // null = following the bottom; otherwise the row pinned to the top.
  const [top, setTop] = useState<number | null>(fromTop ? 0 : null);
  useEffect(() => setTop(fromTop ? 0 : null), [followKey]);

  // Clamp on every render: the maximum changes as content grows or the terminal resizes.
  const maxTop = Math.max(0, content.height - viewport.clientHeight);
  const scrollTop = top === null ? maxTop : Math.min(top, maxTop);
  // Keep one row of overlap so you don't lose your place.
  const page = Math.max(1, viewport.clientHeight - 1);

  // Functional update: a fast wheel spin delivers several notches before the
  // next render, and each must build on the last.
  const scrollBy = (rows: number) =>
    setTop((prev) => {
      const current = prev === null ? maxTop : Math.min(prev, maxTop);
      const next = Math.max(0, current + rows);
      return next >= maxTop ? null : next;
    });

  // The frame Ink draws after this render uses exactly this scrollTop.
  if (!hidden) onViewport?.(
    { top: viewport.top, height: viewport.clientHeight, scrollTop, contentHeight: content.height, width: viewport.width },
    scrollBy,
  );

  // Smooth wheel scrolling: notches add to a distance still to travel, and a
  // per-frame glide covers a third of what's left each step (at least one
  // row). One notch moves 1+1+1 rows instead of jumping 3; a fast flick moves
  // quickly at first, then eases out. Reversing direction drops the rest.
  const latestScrollBy = useRef(scrollBy);
  latestScrollBy.current = scrollBy;
  const glide = useRef({ remaining: 0, timer: null as ReturnType<typeof setTimeout> | null });
  const glideStep = () => {
    const g = glide.current;
    if (g.remaining === 0) {
      g.timer = null;
      return;
    }
    const step = Math.sign(g.remaining) * Math.max(1, Math.ceil(Math.abs(g.remaining) / 3));
    g.remaining -= step;
    latestScrollBy.current(step);
    g.timer = setTimeout(glideStep, GLIDE_MS);
  };
  const glideBy = (rows: number) => {
    const g = glide.current;
    if (Math.sign(rows) !== Math.sign(g.remaining)) g.remaining = 0;
    g.remaining += rows;
    if (!g.timer) glideStep(); // the first row moves right away
  };
  // Unmounting, or being hidden (a subagent's view opening), stops a glide still on its way.
  useEffect(() => {
    if (!hidden) return;
    const g = glide.current;
    if (g.timer) clearTimeout(g.timer);
    g.timer = null;
    g.remaining = 0;
  }, [hidden]);
  useEffect(
    () => () => {
      if (glide.current.timer) clearTimeout(glide.current.timer);
    },
    [],
  );

  useInput(
    (_input, key) => {
      if (key.pageUp) scrollBy(-page);
      else if (key.pageDown) scrollBy(page);
    },
    { isActive },
  );

  // Wheel notches come from src/mouse.ts, which filters them out of stdin.
  // Re-subscribed every render so the handler sees the current maxTop.
  useEffect(() => {
    if (!isActive) return;
    const onMouse = (event: MouseEvent) => {
      if (event.type === "scroll") glideBy(event.step * WHEEL_ROWS);
    };
    mouse.on("event", onMouse);
    return () => {
      mouse.off("event", onMouse);
    };
  });

  return (
    <Box ref={viewportRef} display={hidden ? "none" : "flex"} flexDirection="column" flexGrow={1} flexShrink={1} overflow="hidden" contentOffsetY={scrollTop}>
      {/* flexShrink={0} keeps the content at its natural height, so it can overflow the viewport. */}
      <Box ref={contentRef} flexDirection="column" flexShrink={0}>
        {children}
      </Box>
    </Box>
  );
}
