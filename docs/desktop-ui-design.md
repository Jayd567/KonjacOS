# Desktop UI design notes (for later)

Saved for after Java runs properly. Not started yet.

A windows style taskbar with but instead with things like liquid glass instead of one opaque bar.  
We will avoid rounded rectangles because  the point where a perfect circle arc meets a straight line creates a sudden drop in curvature that the human eye perceives as a harsh tangent line. Instead we should rely on Squircles (Superellipses) or curves with complete curvature continuity (G^2 or G^3 continuity). The mathematical boundary os drawn using Signed Distance Field (SDF). For every pixel coordinate (x,y) the shader evaluates its precise distance to the squircle edge. (with still being performant)

Standard frosted glass just blurs pixels. We will actually bend light like a physical lens. To do this efficiently the shader generates a screen space normal map derived from the spatial gradient (the rate of change) of the shape’s SDF. It takes the X and Y derivatives of the distance field to determine how the “Glass” curves towards the edges.

It then uses Snell’s Law to shift the UV sampling coordinates of the texture behind the window.  
The shader should also dynamically alter its appearance based on whatever background content it is hovering over. It calculates the average color and saturation of the backdrop slice beneath it. If hovering over a vibrant orange wallpaper, the shader dynamically injects a calculated orange tint into the glass matrix to seem cohesive. If the background is dark, the opacity shifts to preserve high contrast.

Furthermore, we should use Variable-Radius Blur to establish depth. A shallow tooltip might only have a 10px blur radius, while a high-level active modal window gets a dense 40px blur, optically signalling to your brain how high the window is floating above the desktop. Perfect digital gradients suffer from an ugly artifact called color banding, visible, blocky steps between shades of color on consumer monitors. To mask this and make the material feel like real physical matter, we will overlay a subtle layer of monochromatic high-frequency noise (at about 2% to 3% opacity.) this acts as a spatial dither filter. The final pass adds a 1px interior border utilizing an asymmetric linear gradient (bright white at the top, transitioning to transparent at the bottom) to simulate a real, physical top-edge light catch.

Though of course we wont have that small little bar like how Apple uses but like what windows has with all your apps down below and not that small little oval down below. This effect will be used across the floating menu bar: the traditional; top-of-screen menu bar is fully transparent. It no longer sits on a solid, opaque stripe. Instead menu dropdowns appear as isolated panes of glass floating directly over the blurred wallpaper. Basically any sort of menu that we have.

TYPOGRAPHY & TEXT RENDERING

Drawing text over a highly dynamic, blurred glass background is difficult. If the text engine isn’t carefully designed, font edges will look jaggy or get swallowed by the shifting colors beneath them.

We will use a Drop Shadow Layer: Plan to draw the text twice when it rests on the glass taskbar or menus. First, draw a blurred highly transparent dark version shifted 1 pixel down ( a soft shadow ) and then draw the crisp white text on top. This creates an optical contrast safety net, ensuring the text remains legible whether it passes over a light or dark wallpaper. 

Subpixel Blitting: Since my kernel already uses embedded bitmap fonts and alpha-blending math (blend\_pixel) plan your userspace font render to use a fractional coverage mask. Instead of treating text as solid “on/off” pixels, use the font’s vector alpha channels to softly blend the text edges directly onto the Liquid Glass shader matrix.

INTERACTION PHYSICS & MOTION

Spring physics for drop downs: When clicking a centered icon on your taskbar to open a menu, don't just make it instantly appear. Plan a simple Hooke’s Law spring-mass system (F=-kx) in your Rust update loop to handle the menu’s entry. Let the glass panel smoothly scale up from the taskbar, slightly overshooting its final size and bouncing gently into place.

Active State Hover Indication: When the cursor hovers over a centered taskbar app, the 8px squircle hover box beneath it shouldn’t just snap to full visibility. Use a linear interpolation loop (lerp) to softly fade the hover box’s alpha opacity from 0% to 15% over a span of 60 to 100 milliseconds.

WINDOW HIERARCHY & INPUT ROUTING

The Z-Index Stack & Hit Testing: Plan a unified layout array in your desktop shell process that tracks every open window from front to back. When a mouse click occurs, the loop tests coordinates starting from the very top window down.

SDF Clipping for Input: Because your windows use an SDF (Signed Distance Field) to draw those beautiful continuous curves, use that exact same SDF function for the mouse click detection. If a user clicks on the very outer edge of a window’s rounded corner, the SDF will instantly tell the system if the click was mathematically inside the squircle or outside on the wallpaper, preventing accidental window activations.

LAYOUT ARCHITECTURE: THE GRID AND DOCK HYBRID

Since I want a windows-style taskbar combined with centered apps and floating dropdowns, I should plan out your desktop real estate coordinates TIGHTLY.

The work area boundary: Ensure your window manager subtract the taskbar’s bounding box height from the screen’s vertical layout coordinates. Maximize actions should expand a window perfectly to fill the space \*above\* the taskbar, rather thank sliding underneath the blurred glass deck.

The global state monitor: Plan a small, lightweight background thread in rust that monitors system metrics (like checking task::TASKS table changes or memory statistics) This thread pipes updates directly into your taskbar’s system tray clock and performance widgets, allowing the text strings inside your liquid-glass containers to stay continuously live without stalling the main UI render loop.

The style across it should stay consistent.

