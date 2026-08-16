#!/usr/bin/env python3
"""Mathematically-perfect looping rotating-globe render.

One revolution over N frames -> frame N == frame 0. All pulses use integer
cycle counts so they too close seamlessly. See report at end of task.
"""
import json, math, os, sys
import numpy as np
from PIL import Image, ImageDraw, ImageFilter

# ---------------------------------------------------------------- config
S        = 2                      # supersample factor
W        = 720                    # final size
WS       = W * S                  # working size
R        = 272 * S                # sphere radius (working px)
CX = CY  = WS // 2
LAT_CAM  = math.radians(18.0)     # camera latitude (look down at N hemisphere)
N        = 288                    # frames (12s @ 24fps)
H_LIFT   = 0.50                   # base arc bulge (scaled by length in slerp_arc)
GRID_DIR = os.path.dirname(os.path.abspath(__file__))

# palette
BG        = (10, 10, 11)
DOT_RGB   = np.array([190, 183, 170], float)   # warm grey continents
GRAT_RGB  = (150, 144, 132)
OXIDE     = np.array([217, 107, 60], float)     # #D96B3C relay orange
OXIDE_HOT = np.array([255, 170, 110], float)    # pulse core

CITIES = {
    "NY":    (40.71,  -74.01),
    "MIA":   (25.76,  -80.19),
    "DAL":   (32.78,  -96.80),
    "DEN":   (39.74, -104.99),
    "SF":    (37.77, -122.42),
    "SEA":   (47.61, -122.33),
    "TOR":   (43.65,  -79.38),
    "MEX":   (19.43,  -99.13),
    "SAO":   (-23.55, -46.63),
    "BA":    (-34.60, -58.38),
    "LON":   (51.51,   -0.13),
    "FRA":   (50.11,    8.68),
    "MAD":   (40.42,   -3.70),
    "LAGOS": (6.52,     3.38),
    "JNB":   (-26.20,  28.05),
    "DXB":   (25.20,   55.27),
    "MUM":   (19.08,   72.88),
    "SGP":   (1.35,   103.82),
    "TOK":   (35.68,  139.77),
    "SYD":   (-33.87, 151.21),
}
# (a, b, pulse_cycles_per_revolution [integer -> loop-safe], phase0, height_factor)
# short regional hops keep low height; long hauls loft. Lift also scales with
# arc length in slerp_arc, so hfac is a per-arc multiplier on top of that.
ARCS = [
    ("NY",    "LON",   2, 0.00, 1.15),   # transatlantic
    ("NY",    "MIA",   2, 0.30, 0.55),   # US east
    ("NY",    "TOR",   3, 0.60, 0.45),   # short
    ("MIA",   "DAL",   2, 0.10, 0.55),
    ("DAL",   "DEN",   2, 0.70, 0.50),
    ("DEN",   "SF",    2, 0.20, 0.60),
    ("SF",    "SEA",   3, 0.80, 0.45),   # short
    ("MEX",   "SF",    2, 0.15, 0.70),
    ("SEA",   "TOK",   1, 0.40, 1.25),   # transpacific
    ("SF",    "TOK",   1, 0.05, 1.25),   # transpacific
    ("MIA",   "SAO",   1, 0.55, 1.00),
    ("SAO",   "BA",    2, 0.25, 0.55),
    ("SAO",   "LAGOS", 2, 0.65, 1.00),   # south atlantic
    ("LAGOS", "JNB",   2, 0.15, 0.70),
    ("MAD",   "LAGOS", 2, 0.85, 0.80),
    ("LON",   "FRA",   3, 0.35, 0.45),   # short
    ("FRA",   "MAD",   2, 0.50, 0.60),
    ("LON",   "DXB",   1, 0.10, 1.05),
    ("JNB",   "DXB",   1, 0.50, 0.95),
    ("DXB",   "MUM",   3, 0.60, 0.55),
    ("MUM",   "SGP",   2, 0.30, 0.85),
    ("SGP",   "TOK",   2, 0.75, 0.90),
    ("SGP",   "SYD",   1, 0.45, 0.95),
    ("TOK",   "SYD",   1, 0.90, 1.00),
]

# ---------------------------------------------------------------- geometry
def ll_to_vec(lat, lon):
    la, lo = math.radians(lat), math.radians(lon)
    return np.array([math.cos(la)*math.cos(lo),
                     math.cos(la)*math.sin(lo),
                     math.sin(la)])

def rot_project(vecs, lon0):
    """vecs (N,3) world -> screen. Returns sx, sy, depth (facing>0), topf."""
    c, s = math.cos(-lon0), math.sin(-lon0)
    Rz = np.array([[c, -s, 0],[s, c, 0],[0,0,1]])
    ca, sa = math.cos(LAT_CAM), math.sin(LAT_CAM)
    Ry = np.array([[ca, 0, sa],[0,1,0],[-sa,0,ca]])
    v = vecs @ Rz.T @ Ry.T
    depth = v[:,0]                       # toward camera (+x)
    sx = CX + R * v[:,1]
    sy = CY - R * v[:,2]
    topf = (v[:,2] + 1.0) * 0.5
    return sx, sy, depth, topf

# ---------------------------------------------------------------- land dots
def build_land_dots():
    gj = json.load(open(os.path.join(GRID_DIR, "ne_110m_land.geojson")))
    polys = []
    for feat in gj["features"]:
        g = feat["geometry"]
        if g["type"] == "Polygon":
            polys.append(g["coordinates"])
        elif g["type"] == "MultiPolygon":
            polys.extend(g["coordinates"])

    def point_in_ring(lon, lat, ring):
        inside = False
        n = len(ring); j = n - 1
        for i in range(n):
            xi, yi = ring[i][0], ring[i][1]
            xj, yj = ring[j][0], ring[j][1]
            if ((yi > lat) != (yj > lat)) and \
               (lon < (xj - xi) * (lat - yi) / (yj - yi + 1e-12) + xi):
                inside = not inside
            j = i
        return inside

    def is_land(lon, lat):
        for poly in polys:
            if point_in_ring(lon, lat, poly[0]):
                hole = False
                for h in poly[1:]:
                    if point_in_ring(lon, lat, h):
                        hole = True; break
                if not hole:
                    return True
        return False

    step = 2.0
    vecs = []
    lat = -78.0
    while lat <= 84.0:
        # even areal density: lon step scaled by 1/cos(lat)
        cl = max(math.cos(math.radians(lat)), 0.10)
        lon_step = step / cl
        lon = -180.0
        while lon < 180.0:
            if is_land(lon, lat):
                vecs.append(ll_to_vec(lat, lon))
            lon += lon_step
        lat += step
    return np.array(vecs)

# ---------------------------------------------------------------- graticule
def build_graticule():
    """List of polylines (each an (M,3) array of vecs)."""
    lines = []
    for lon in range(-180, 180, 30):
        pts = [ll_to_vec(la, lon) for la in np.arange(-90, 90.1, 3)]
        lines.append(np.array(pts))
    for lat in range(-60, 61, 30):
        pts = [ll_to_vec(lat, lo) for lo in np.arange(-180, 180.1, 3)]
        lines.append(np.array(pts))
    return lines

# ---------------------------------------------------------------- arcs
def slerp_arc(a, b, hfac, m=140):
    va, vb = ll_to_vec(*CITIES[a]), ll_to_vec(*CITIES[b])
    dot = np.clip(np.dot(va, vb), -1, 1)
    om = math.acos(dot)
    # length-scaled height: short hops stay tight, long hauls loft
    h = H_LIFT * min(om / (math.pi * 0.85), 1.0) * hfac
    us = np.linspace(0, 1, m)
    pts = np.zeros((m, 3))
    for i, u in enumerate(us):
        if om < 1e-6:
            v = va
        else:
            v = (math.sin((1-u)*om)*va + math.sin(u*om)*vb) / math.sin(om)
        lift = 1.0 + h * math.sin(math.pi * u)
        pts[i] = v * lift
    return pts, us

ARC_CACHE = [(slerp_arc(a, b, hf), (a, b, k, ph)) for (a, b, k, ph, hf) in ARCS]

# ---------------------------------------------------------------- draw
LAND = None
GRAT = None
STATIC = None   # bg + stars + dark sphere disc (frame-invariant)
GA = None       # top-glow field
VIG = None      # vignette field

def smoothstep(e0, e1, x):
    t = np.clip((x - e0) / (e1 - e0), 0, 1)
    return t * t * (3 - 2 * t)

def build_static():
    global STATIC, GA, VIG
    # top glow field
    glowimg = Image.new("L", (WS, WS), 0)
    ImageDraw.Draw(glowimg).ellipse([CX-R, CY-R, CX+R, CY+R], fill=40)
    glowimg = glowimg.filter(ImageFilter.GaussianBlur(R*0.32))
    GA = np.asarray(glowimg, float) / 255.0
    # vignette field
    yy, xx = np.mgrid[0:WS, 0:WS]
    d = np.sqrt((xx-CX)**2 + (yy-CY)**2) / (WS*0.72)
    VIG = np.clip(1.0 - 0.55*np.power(d, 2.2), 0.35, 1.0)
    # static base image
    base = Image.new("RGB", (WS, WS), BG)
    bd = ImageDraw.Draw(base, "RGBA")
    rng = np.random.default_rng(7)
    for _ in range(90):
        x = rng.uniform(0, WS); y = rng.uniform(0, WS)
        if (x-CX)**2 + (y-CY)**2 < (R*1.02)**2:
            continue
        b = rng.uniform(30, 90)
        bd.ellipse([x-1*S, y-1*S, x+1*S, y+1*S], fill=(int(b),int(b),int(b*0.95),255))
    bd.ellipse([CX-R, CY-R, CX+R, CY+R], fill=(14, 14, 16, 255))
    STATIC = base

def render_frame(f):
    global LAND, GRAT
    if LAND is None:
        LAND = build_land_dots()
    if GRAT is None:
        GRAT = build_graticule()
    if STATIC is None:
        build_static()
    lon0 = 2 * math.pi * f / N

    base = STATIC.copy()
    bd = ImageDraw.Draw(base, "RGBA")
    orange = Image.new("RGB", (WS, WS), (0, 0, 0))
    od = ImageDraw.Draw(orange, "RGBA")
    ga = GA

    # --- graticule (front hemisphere only)
    for line in GRAT:
        sx, sy, dep, _ = rot_project(line, lon0)
        pts = []
        for i in range(len(sx)):
            if dep[i] > 0.02:
                pts.append((sx[i], sy[i]))
            else:
                if len(pts) > 1:
                    bd.line(pts, fill=(*GRAT_RGB, 42), width=max(1, S//2))
                pts = []
        if len(pts) > 1:
            bd.line(pts, fill=(*GRAT_RGB, 42), width=max(1, S//2))

    # --- land dots
    sx, sy, dep, topf = rot_project(LAND, lon0)
    vis = dep > 0.0
    bright = np.clip(0.28 + 0.55*np.power(np.clip(dep,0,1), 0.6) + 0.30*topf, 0, 1)
    r_dot = 1.5 * S
    for i in range(len(sx)):
        if not vis[i]:
            continue
        b = bright[i]
        col = (DOT_RGB * b).astype(int)
        a = int(70 + 150 * b)
        bd.ellipse([sx[i]-r_dot, sy[i]-r_dot, sx[i]+r_dot, sy[i]+r_dot],
                   fill=(int(col[0]), int(col[1]), int(col[2]), a))

    # --- sphere limb (thin rim + bright bottom crescent)
    bd.ellipse([CX-R, CY-R, CX+R, CY+R], outline=(210, 200, 185, 90),
               width=max(1, S))
    # bright bottom rim: draw arc glow on orange->actually warm white, add to base glow
    rim = Image.new("L", (WS, WS), 0)
    rd = ImageDraw.Draw(rim)
    rd.arc([CX-R, CY-R, CX+R, CY+R], start=25, end=155, fill=255, width=max(2, 2*S))
    rim = rim.filter(ImageFilter.GaussianBlur(6*S))
    rimarr = np.asarray(rim, float) / 255.0

    # ---------- ORANGE layer: arcs, pulses, nodes ----------
    node_boost = {c: 0.0 for c in CITIES}
    for (pts, us), (a, b, k, ph) in ARC_CACHE:
        sx, sy, dep, _ = rot_project(pts, lon0)
        za = dep[0]; zb = dep[-1]
        vis_a = smoothstep(-0.05, 0.18, za)
        vis_b = smoothstep(-0.05, 0.18, zb)
        arc_vis = min(vis_a, vis_b)
        if arc_vis <= 0.01:
            continue
        alpha = int(150 * arc_vis)
        seg = []
        for i in range(len(sx)):
            if dep[i] > -0.02:
                seg.append((sx[i], sy[i]))
            else:
                if len(seg) > 1:
                    od.line(seg, fill=(int(OXIDE[0]),int(OXIDE[1]),int(OXIDE[2]),alpha), width=max(1,S))
                seg = []
        if len(seg) > 1:
            od.line(seg, fill=(int(OXIDE[0]),int(OXIDE[1]),int(OXIDE[2]),alpha), width=max(1,S))

        # traveling pulse (integer cycles -> loop safe)
        pu = (k * f / N + ph) % 1.0
        idx = int(round(pu * (len(us) - 1)))
        if dep[idx] > 0.05:
            px, py = sx[idx], sy[idx]
            pr = 3.2 * S * arc_vis
            od.ellipse([px-pr, py-pr, px+pr, py+pr],
                       fill=(int(OXIDE_HOT[0]),int(OXIDE_HOT[1]),int(OXIDE_HOT[2]),int(230*arc_vis)))
            # short comet trail
            for t in range(1, 7):
                j = max(0, idx - t*2)
                if dep[j] > 0.05:
                    tr = pr * (1 - t/7.0)
                    od.ellipse([sx[j]-tr, sy[j]-tr, sx[j]+tr, sy[j]+tr],
                               fill=(int(OXIDE[0]),int(OXIDE[1]),int(OXIDE[2]),int(120*arc_vis*(1-t/7.0))))
        # node arrival bloom (pulse near endpoint), integer phased
        arrival = 0.5 - 0.5*math.cos(2*math.pi*(k*f/N + ph))  # 0..1, integer cycles
        node_boost[b] = max(node_boost[b], arrival * arc_vis)
        node_boost[a] = max(node_boost[a], (1-arrival) * 0.5 * arc_vis)

    # relay nodes
    for c, (lat, lon) in CITIES.items():
        v = ll_to_vec(lat, lon)[None, :]
        sx, sy, dep, _ = rot_project(v, lon0)
        if dep[0] <= 0.02:
            continue
        vfac = smoothstep(0.02, 0.2, dep[0])
        boost = node_boost[c]
        rr = (2.6 + 2.2*boost) * S * vfac
        od.ellipse([sx[0]-rr, sy[0]-rr, sx[0]+rr, sy[0]+rr],
                   fill=(int(OXIDE_HOT[0]),int(OXIDE_HOT[1]),int(OXIDE_HOT[2]),int((200)*vfac)))
        od.ellipse([sx[0]-rr*0.5, sy[0]-rr*0.5, sx[0]+rr*0.5, sy[0]+rr*0.5],
                   fill=(255,225,200,int(230*vfac)))

    # ---------- COMPOSITE ----------
    b = np.asarray(base, float)
    o = np.asarray(orange, float)
    og = Image.fromarray(o.astype(np.uint8))
    blur_t = np.asarray(og.filter(ImageFilter.GaussianBlur(3*S)), float)
    blur_w = np.asarray(og.filter(ImageFilter.GaussianBlur(11*S)), float)

    out = b.copy()
    # top glow (warm white) additive
    warm = np.array([255, 236, 210], float)
    out += ga[:, :, None] * warm * 0.55
    # bottom limb glow (warm white)
    out += rimarr[:, :, None] * warm * 0.5
    # orange additive bloom
    out += o * 1.0 + blur_t * 0.9 + blur_w * 0.7
    out = np.clip(out, 0, 255)

    # vignette
    out *= VIG[:, :, None]
    out = np.clip(out, 0, 255).astype(np.uint8)

    img = Image.fromarray(out).resize((W, W), Image.LANCZOS)
    return img

# ---------------------------------------------------------------- main
if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "one":
        f = int(sys.argv[2]) if len(sys.argv) > 2 else 0
        render_frame(f).save(os.path.join(GRID_DIR, f"test_{f:03d}.png"))
        print("wrote test frame", f)
    else:
        outdir = os.path.join(GRID_DIR, "frames")
        for f in range(N):
            render_frame(f).save(os.path.join(outdir, f"f{f:04d}.png"))
            if f % 24 == 0:
                print("frame", f)
        print("done", N)
