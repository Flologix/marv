// Marv the martian (docs/images/marv.svg), without its tile: drawn in the current text colour.
export function Martian({ size = 64, className }: { size?: number; className?: string }) {
  return (
    <svg
      className={className}
      xmlns="http://www.w3.org/2000/svg"
      viewBox="2 2 11 13"
      width={size}
      height={(size * 13) / 11}
      shapeRendering="crispEdges"
      aria-hidden
    >
      <g fill="currentColor">
          <circle cx="3.5" cy="3" r="0.45"/>
          <circle cx="11.5" cy="3" r="0.45"/>
          <rect x="4" y="4" width="1" height="1"/>
          <rect x="5" y="5" width="1" height="1"/>
          <rect x="9" y="5" width="1" height="1"/>
          <rect x="10" y="4" width="1" height="1"/>
          <rect x="3" y="7" width="1" height="1"/>
          <rect x="4" y="6" width="1" height="1"/>
          <rect x="4" y="7" width="1" height="1"/>
          <rect x="5" y="6" width="1" height="1"/>
          <rect x="5" y="7" width="1" height="1"/>
          <rect x="6" y="6" width="1" height="1"/>
          <rect x="6" y="7" width="1" height="1"/>
          <rect x="7" y="6" width="1" height="1"/>
          <rect x="7" y="7" width="1" height="1"/>
          <rect x="8" y="6" width="1" height="1"/>
          <rect x="8" y="7" width="1" height="1"/>
          <rect x="9" y="6" width="1" height="1"/>
          <rect x="9" y="7" width="1" height="1"/>
          <rect x="10" y="6" width="1" height="1"/>
          <rect x="10" y="7" width="1" height="1"/>
          <rect x="11" y="7" width="1" height="1"/>
          <rect x="2" y="9" width="1" height="1"/>
          <rect x="3" y="8" width="1" height="1"/>
          <rect x="3" y="9" width="1" height="1"/>
          <rect x="4" y="8" width="1" height="1"/>
          <rect x="4" y="9" width="1" height="1"/>
          <rect x="6" y="8" width="1" height="1"/>
          <rect x="6" y="9" width="1" height="1"/>
          <rect x="7" y="8" width="1" height="1"/>
          <rect x="7" y="9" width="1" height="1"/>
          <rect x="8" y="8" width="1" height="1"/>
          <rect x="8" y="9" width="1" height="1"/>
          <rect x="10" y="8" width="1" height="1"/>
          <rect x="10" y="9" width="1" height="1"/>
          <rect x="11" y="8" width="1" height="1"/>
          <rect x="11" y="9" width="1" height="1"/>
          <rect x="12" y="9" width="1" height="1"/>
          <rect x="3" y="10" width="1" height="1"/>
          <rect x="3" y="11" width="1" height="1"/>
          <rect x="4" y="10" width="1" height="1"/>
          <rect x="4" y="11" width="1" height="1"/>
          <rect x="5" y="10" width="1" height="1"/>
          <rect x="5" y="11" width="1" height="1"/>
          <rect x="6" y="10" width="1" height="1"/>
          <rect x="7" y="10" width="1" height="1"/>
          <rect x="8" y="10" width="1" height="1"/>
          <rect x="9" y="10" width="1" height="1"/>
          <rect x="9" y="11" width="1" height="1"/>
          <rect x="10" y="10" width="1" height="1"/>
          <rect x="10" y="11" width="1" height="1"/>
          <rect x="11" y="10" width="1" height="1"/>
          <rect x="11" y="11" width="1" height="1"/>
          <rect x="3" y="12" width="1" height="1"/>
          <rect x="4" y="12" width="1" height="1"/>
          <rect x="4" y="13" width="1" height="1"/>
          <rect x="5" y="12" width="1" height="1"/>
          <rect x="5" y="13" width="1" height="1"/>
          <rect x="6" y="12" width="1" height="1"/>
          <rect x="6" y="13" width="1" height="1"/>
          <rect x="7" y="12" width="1" height="1"/>
          <rect x="7" y="13" width="1" height="1"/>
          <rect x="8" y="12" width="1" height="1"/>
          <rect x="8" y="13" width="1" height="1"/>
          <rect x="9" y="12" width="1" height="1"/>
          <rect x="9" y="13" width="1" height="1"/>
          <rect x="10" y="12" width="1" height="1"/>
          <rect x="10" y="13" width="1" height="1"/>
          <rect x="11" y="12" width="1" height="1"/>
          <rect x="4" y="14" width="1" height="1"/>
          <rect x="5" y="14" width="1" height="1"/>
          <rect x="9" y="14" width="1" height="1"/>
          <rect x="10" y="14" width="1" height="1"/>
        </g>
    </svg>
  );
}
