"""Rebuild Crosspane's vector identity and exports with Python, Pillow and rsvg-convert.

No fonts are required: the wordmark and endorsement are drawn as paths.
Generated artwork is kept separately and is never overwritten by this script.
"""
from pathlib import Path
import subprocess
from PIL import Image

ROOT = Path(__file__).resolve().parent
NAVY, CYAN, ICE, WHITE = '#164a74', '#17c8f4', '#6fdcff', '#e9f8ff'
# Faceted crossing-C, a simplified vector companion to the generated sculptural emblem.
OUTLINE = '236,40 438,102 383,196 228,138 155,199 253,244 200,306 158,332 225,402 380,302 450,389 227,462 71,365 71,144'
FACETS = [
    ('236,40 438,102 364,125 211,86', '#b7efff'),
    ('236,40 211,86 71,144', '#17c8f4'),
    ('211,86 364,125 383,196 228,138 188,101', '#0877b4'),
    ('364,125 438,102 383,196', '#63ddff'),
    ('71,144 188,101 155,199 71,269', '#39c9f2'),
    ('71,144 253,244 195,274 128,209', '#b7efff'),
    ('195,274 253,244 200,306 71,365 71,326', '#16bce9'),
    ('71,269 128,239 128,285 71,326', '#0b416d'),
    ('128,285 200,306 158,332 225,402 154,417 71,365', '#0877b4'),
    ('71,365 154,417 227,462 140,424', '#17c8f4'),
    ('154,417 225,402 380,302 381,360 227,462', '#64ddff'),
    ('380,302 450,389 381,360', '#d9f7ff'),
    ('381,360 450,389 227,462', '#07507e'),
]

def mark(mono=None):
    outline=f'<polygon points="{OUTLINE}" fill="{mono or CYAN}"/>'
    if mono:
        return outline
    return outline+''.join(f'<polygon points="{pts}" fill="{color}"/>' for pts,color in FACETS)

GLYPHS = {
'C':['72,8 24,8 8,24 8,76 24,92 72,92'],
'R':['8,100 8,8 56,8 72,24 72,40 56,56 8,56','42,56 76,100'],
'O':['24,8 56,8 72,24 72,76 56,92 24,92 8,76 8,24 24,8'],
'S':['72,8 24,8 8,24 8,40 24,50 56,50 72,64 72,76 56,92 8,92'],
'P':['8,100 8,8 56,8 72,24 72,40 56,56 8,56'],
'A':['8,100 36,8 48,8 76,100','20,64 64,64'],
'N':['8,100 8,0 72,100 72,0'],
'E':['72,8 8,8 8,92 72,92','8,50 56,50'],
'B':['8,100 8,8 56,8 72,24 72,36 56,50 8,50','56,50 72,64 72,76 56,92 8,92'],
'Y':['8,0 40,50 72,0','40,50 40,100'],
'F':['8,100 8,8 72,8','8,50 56,50'],
'T':['0,8 80,8','40,8 40,100'],
'D':['8,100 8,8 48,8 72,32 72,68 48,92 8,92'],
'V':['8,0 36,92 48,92 76,0'],
}

def lettering(text, color, weight=14):
    out=[]
    for i,c in enumerate(text):
        for pts in GLYPHS.get(c,[]):
            out.append(f'<polyline transform="translate({i*100} 0)" points="{pts}" fill="none" stroke="{color}" stroke-width="{weight}" stroke-linejoin="bevel"/>')
    return ''.join(out)

def svg(name,w,h,body,label):
    path=ROOT/name
    path.write_text(f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" aria-label="{label}"><title>{label}</title>{body}</svg>\n')
    return path

def raster(source,name,size):
    subprocess.run(['rsvg-convert','-w',str(size),'-o',str(ROOT/name),str(source)],check=True)

for variant,color in [('color',None),('white',WHITE),('navy',NAVY),('black','#000000')]:
    src=svg(f'crosspane-mark-{variant}.svg',512,512,mark(color),'Crosspane crystalline crossing')
    raster(src,f'crosspane-mark-{variant}.png',1024)
    svg(f'crosspane-tray-{variant}.svg',32,32,f'<g transform="scale(.0625)">{mark(color)}</g>','Crosspane')

for variant,color,sub in [('dark',WHITE,ICE),('light',NAVY,NAVY),('mono','#ffffff','#ffffff')]:
    body=f'<g transform="translate(14 18) scale(.53)">{mark(None if variant=="dark" else color)}</g>'
    body+=f'<g transform="translate(330 66) scale(1.34)">{lettering("CROSSPANE",color)}</g>'
    body+=f'<g transform="translate(338 250) scale(.18)">{lettering("BY FROSTDEV",sub,10)}</g>'
    src=svg(f'crosspane-lockup-{variant}.svg',1560,310,body,'Crosspane by Frostdev')
    raster(src,f'crosspane-lockup-{variant}.png',2340)
    svg(f'crosspane-wordmark-{variant}.svg',1220,170,f'<g transform="translate(10 16) scale(1.34)">{lettering("CROSSPANE",color)}</g>','Crosspane')

icon=svg('crosspane-icon.svg',512,512,
    '<defs><linearGradient id="bg" x2="1" y2="1"><stop stop-color="#163953"/><stop offset="1" stop-color="#071525"/></linearGradient></defs>'
    '<rect x="8" y="8" width="496" height="496" rx="110" fill="url(#bg)"/>'
    '<rect x="9" y="9" width="494" height="494" rx="109" fill="none" stroke="#386079" stroke-width="2"/>'
    f'<g transform="translate(52 52) scale(.8)">{mark()}</g>', 'Crosspane application icon')
for size in [16,24,32,48,64,128,180,256,512,1024]:
    raster(icon,f'crosspane-icon-{size}.png',size)
Image.open(ROOT/'crosspane-icon-256.png').save(ROOT/'crosspane.ico',sizes=[(16,16),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)])
Image.open(ROOT/'crosspane-icon-1024.png').save(ROOT/'crosspane.icns')
Image.open(ROOT/'crosspane-icon-180.png').save(ROOT/'apple-touch-icon.png')
Image.open(ROOT/'crosspane-icon-32.png').save(ROOT/'favicon.png')
# Preview sheet keeps every treatment and export visually reviewable.
svg('identity-sheet.svg',1600,1100,
    '<rect width="1600" height="1100" fill="#071525"/>'
    '<g transform="translate(95 50) scale(.9)">'+(ROOT/'crosspane-lockup-dark.svg').read_text().split('<title>Crosspane by Frostdev</title>')[1].split('</svg>')[0]+'</g>'
    '<path d="M90 370H1510" stroke="#24425c"/>'
    f'<g transform="translate(120 440) scale(.7)">{mark()}</g>'
    f'<g transform="translate(630 440) scale(.7)">{mark(WHITE)}</g>'
    '<rect x="1120" y="440" width="360" height="360" rx="70" fill="#e9f8ff"/>'
    f'<g transform="translate(1120 440) scale(.7)">{mark(NAVY)}</g>'
    +''.join(f'<rect x="{100+i*290}" y="940" width="260" height="80" rx="10" fill="{c}"/>' for i,c in enumerate(['#071525',NAVY,CYAN,ICE,WHITE])),
    'Crosspane identity: lockup, crystalline crossing, monochrome marks and color palette')
raster(ROOT/'identity-sheet.svg','identity-sheet.png',1600)
print('Built vector marks, wordmarks, lockups, tray symbols, app icons, ICO, ICNS and identity sheet.')

# Code-native promotional layouts use the same portable vector identity.
for name,w,h in [('crosspane-social',1280,640),('crosspane-banner',1600,560)]:
    body=f'<rect width="{w}" height="{h}" fill="#071525"/>'
    body+='<defs><radialGradient id="glow"><stop stop-color="#164a74"/><stop offset="1" stop-color="#071525"/></radialGradient></defs>'
    body+=f'<ellipse cx="{w*.79}" cy="{h*.46}" rx="{h*.62}" ry="{h*.65}" fill="url(#glow)"/>'
    # Rhythmic folded paths echo the crossing ribbon without adding another logo.
    body+=f'<path d="M0 {h*.88} L{w*.5} {h*.66} L{w} {h*.85} M0 {h*.95} L{w*.5} {h*.73} L{w} {h*.92}" fill="none" stroke="#164a74" stroke-width="2"/>'
    body+=f'<g transform="translate({w*.64} {h*.09}) scale({h*.0015})">{mark()}</g>'
    body+=f'<g transform="translate(65 {h*.36}) scale(.72)">{lettering("CROSSPANE",WHITE)}</g>'
    body+=f'<text x="70" y="{h*.61}" fill="#89cbd5" font-family="sans-serif" font-size="{h*.04}">Your computers. One workspace.</text>'
    body+=f'<g transform="translate(70 {h*.77}) scale(.13)">{lettering("BY FROSTDEV",ICE,10)}</g>'
    path=svg(name+'.svg',w,h,body,'Crosspane — Your computers. One workspace.')
    raster(path,name+'.png',w)

art=ROOT/'crosspane-wallpaper.png'
if art.exists():
    img=Image.open(art).convert('RGB')
    img.save(ROOT/'crosspane-hero.jpg',quality=92,optimize=True)
    img.thumbnail((1200,675),Image.Resampling.LANCZOS)
    img.save(ROOT/'crosspane-hero.webp',quality=90,method=6)
print('Built social card, banner, optimized README hero and WebP export.')
