#import <AppKit/AppKit.h>

typedef void (*ChromeWake)(const void *);

// These public selectors are resolved at runtime so an older build SDK can still
// use Liquid Glass when the app runs on macOS 26 or later.
@protocol BitChordGlass
@property (nonatomic, strong) NSView *contentView;
@property (nonatomic) CGFloat cornerRadius;
@optional
@property (nonatomic) BOOL effectIsInteractive;
@end

@interface BitChordHistoryButton : NSButton
@property (nonatomic, strong) NSView *hoverFill;
@property (nonatomic, strong) NSTrackingArea *hoverTracking;
@end

@implementation BitChordHistoryButton

- (void)updateTrackingAreas {
    [super updateTrackingAreas];
    if (self.hoverTracking) [self removeTrackingArea:self.hoverTracking];
    self.hoverTracking = [[NSTrackingArea alloc] initWithRect:NSZeroRect
        options:NSTrackingMouseEnteredAndExited | NSTrackingActiveInKeyWindow | NSTrackingInVisibleRect
        owner:self userInfo:nil];
    [self addTrackingArea:self.hoverTracking];
}

- (void)setHovered:(BOOL)hovered {
    [NSAnimationContext runAnimationGroup:^(NSAnimationContext *context) {
        context.duration = NSWorkspace.sharedWorkspace.accessibilityDisplayShouldReduceMotion ? 0 : 0.12;
        self.hoverFill.animator.alphaValue = hovered && self.enabled ? 1 : 0;
    } completionHandler:nil];
}

- (void)mouseEntered:(NSEvent *)event { [self setHovered:YES]; }
- (void)mouseExited:(NSEvent *)event { [self setHovered:NO]; }

- (void)viewDidChangeEffectiveAppearance {
    [super viewDidChangeEffectiveAppearance];
    [self.effectiveAppearance performAsCurrentDrawingAppearance:^{
        self.hoverFill.layer.backgroundColor = [NSColor.labelColor colorWithAlphaComponent:0.10].CGColor;
    }];
}

- (void)setEnabled:(BOOL)enabled {
    if (self.enabled == enabled) return;
    [super setEnabled:enabled];
    NSPoint mouse = [self convertPoint:self.window.mouseLocationOutsideOfEventStream fromView:nil];
    [self setHovered:enabled && self.window.isKeyWindow && NSPointInRect(mouse, self.bounds)];
}

@end

@interface BitChordChrome : NSObject
@property (nonatomic, weak) NSView *content;
@property (nonatomic, strong) id titlebarMonitor;
@property (nonatomic) NSRect beforeFill;
@property (nonatomic, strong) NSView *history;
@property (nonatomic, strong) NSView *historyContents;
@property (nonatomic, strong) BitChordHistoryButton *back;
@property (nonatomic, strong) BitChordHistoryButton *forward;
@property (nonatomic, strong) id accessibilityObserver;
@property (nonatomic) const void *wakeContext;
@property (nonatomic) ChromeWake wake;
@property (nonatomic) unsigned int pending;
@end

@implementation BitChordChrome

- (BitChordHistoryButton *)historyButton:(NSString *)label symbol:(NSString *)symbol action:(SEL)action {
    NSImage *image = [NSImage imageWithSystemSymbolName:symbol accessibilityDescription:label];
    image = [image imageWithSymbolConfiguration:
        [NSImageSymbolConfiguration configurationWithPointSize:14 weight:NSFontWeightSemibold]];
    BitChordHistoryButton *button = [BitChordHistoryButton new];
    button.title = @"";
    button.image = image;
    button.imagePosition = NSImageOnly;
    [button setButtonType:NSButtonTypeMomentaryChange];
    button.bordered = NO;
    button.target = self;
    button.action = action;
    button.toolTip = label;
    button.accessibilityLabel = label;
    button.hoverFill = [NSView new];
    button.hoverFill.wantsLayer = YES;
    button.hoverFill.layer.backgroundColor = [NSColor.labelColor colorWithAlphaComponent:0.10].CGColor;
    button.hoverFill.alphaValue = 0;
    return button;
}

- (BOOL)handleTitlebarEvent:(NSEvent *)event {
    NSView *view = self.content;
    NSWindow *window = view.window;
    if (!window || event.window != window || !(window.styleMask & NSWindowStyleMaskFullSizeContentView)
        || (window.styleMask & NSWindowStyleMaskFullScreen)) return NO;

    NSPoint point = event.locationInWindow;
    NSRect layout = window.contentLayoutRect;
    NSRect bounds = [view convertRect:view.bounds toView:nil];
    // Leave the outer resize edge and the genuine window controls to AppKit.
    if (!NSPointInRect(point, NSInsetRect(bounds, 4, 3)) || point.y < NSMaxY(layout)) return NO;
    for (NSWindowButton kind = NSWindowCloseButton; kind <= NSWindowZoomButton; kind++) {
        NSButton *button = [window standardWindowButton:kind];
        if (button && NSPointInRect(point, [button convertRect:button.bounds toView:nil])) return NO;
    }

    if (event.clickCount == 1) {
        [window performWindowDragWithEvent:event];
    } else if (event.clickCount == 2) {
        [self doubleClickTitlebar:window];
    }
    return YES;
}

- (void)doubleClickTitlebar:(NSWindow *)window {
    // Full-size content receives clicks in the transparent titlebar. Match the
    // user's Desktop & Dock preference for the reserved drag region.
    NSUserDefaults *defaults = NSUserDefaults.standardUserDefaults;
    NSString *action = [defaults stringForKey:@"AppleActionOnDoubleClick"];
    if ([action isEqualToString:@"Minimize"] ||
        (!action && [defaults boolForKey:@"AppleMiniaturizeOnDoubleClick"])) {
        [window performMiniaturize:self];
    } else if ([action isEqualToString:@"Maximize"] && window.screen) {
        NSRect visible = window.screen.visibleFrame;
        BOOL restore = NSEqualRects(window.frame, visible) && !NSIsEmptyRect(self.beforeFill);
        if (!restore) self.beforeFill = window.frame;
        [window setFrame:restore ? self.beforeFill : visible display:YES
                 animate:!NSWorkspace.sharedWorkspace.accessibilityDisplayShouldReduceMotion];
        if (restore) self.beforeFill = NSZeroRect;
    } else if (!action || [action isEqualToString:@"Zoom"] || [action isEqualToString:@"Maximize"]) {
        [window performZoom:self];
    }
}

- (void)activate:(unsigned int)direction {
    self.pending |= direction;
    // Return keyboard input to egui after the native action.
    [self.content.window makeFirstResponder:self.content];
    self.wake(self.wakeContext);
}

- (void)goBack:(id)sender {
    [self activate:1];
}

- (void)goForward:(id)sender {
    [self activate:2];
}

@end

static void installWindowMenu(void) {
    if (!NSApp.mainMenu || NSApp.windowsMenu) return;
    NSMenu *menu = [[NSMenu alloc] initWithTitle:@"Window"];
    [menu addItemWithTitle:@"Close Window" action:@selector(performClose:) keyEquivalent:@"w"];
    [menu addItemWithTitle:@"Minimize" action:@selector(performMiniaturize:) keyEquivalent:@"m"];
    [menu addItemWithTitle:@"Zoom" action:@selector(performZoom:) keyEquivalent:@""];
    NSMenuItem *fullscreen = [menu addItemWithTitle:@"Toggle Full Screen"
        action:@selector(toggleFullScreen:) keyEquivalent:@"f"];
    fullscreen.keyEquivalentModifierMask = NSEventModifierFlagControl | NSEventModifierFlagCommand;
    NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:menu.title action:NULL keyEquivalent:@""];
    item.submenu = menu;
    [NSApp.mainMenu addItem:item];
    NSApp.windowsMenu = menu;
}

void *bitchord_chrome_create(void *view, ChromeWake wake, const void *wakeContext) {
    NSView *content = (__bridge NSView *)view;
    if (!content.window) return NULL;

    BitChordChrome *chrome = [BitChordChrome new];
    chrome.content = content;
    chrome.wake = wake;
    chrome.wakeContext = wakeContext;
    chrome.historyContents = [NSView new];
    Class glassClass = NSClassFromString(@"NSGlassEffectView");
    if (glassClass) {
        NSView<BitChordGlass> *glass = [glassClass new];
        glass.contentView = chrome.historyContents;
        if ([glass respondsToSelector:@selector(setEffectIsInteractive:)]) {
            glass.effectIsInteractive = YES;
        }
        chrome.history = glass;
    } else {
        NSVisualEffectView *material = [NSVisualEffectView new];
        material.material = NSVisualEffectMaterialTitlebar;
        material.blendingMode = NSVisualEffectBlendingModeWithinWindow;
        material.wantsLayer = YES;
        material.layer.masksToBounds = YES;
        [material addSubview:chrome.historyContents];
        chrome.history = material;
    }
    chrome.history.hidden = YES;
    chrome.back = [chrome historyButton:@"Back" symbol:@"chevron.backward" action:@selector(goBack:)];
    chrome.forward = [chrome historyButton:@"Forward" symbol:@"chevron.forward" action:@selector(goForward:)];
    for (BitChordHistoryButton *button in @[chrome.back, chrome.forward]) {
        [chrome.historyContents addSubview:button.hoverFill];
        [chrome.historyContents addSubview:button];
    }
    [content addSubview:chrome.history];
    content.window.titlebarSeparatorStyle = NSTitlebarSeparatorStyleNone;
    content.window.tabbingMode = NSWindowTabbingModeDisallowed;
    installWindowMenu();

    __weak BitChordChrome *weakChrome = chrome;
    chrome.titlebarMonitor = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskLeftMouseDown
        handler:^NSEvent *(NSEvent *event) {
            return [weakChrome handleTitlebarEvent:event] ? nil : event;
        }];
    chrome.accessibilityObserver = [NSWorkspace.sharedWorkspace.notificationCenter
        addObserverForName:NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification
        object:nil queue:NSOperationQueue.mainQueue usingBlock:^(NSNotification *notification) {
            (void)notification;
            BitChordChrome *current = weakChrome;
            if (current) current.wake(current.wakeContext);
        }];
    return (__bridge_retained void *)chrome;
}

void bitchord_chrome_destroy(void *handle) {
    BitChordChrome *chrome = (__bridge_transfer BitChordChrome *)handle;
    [NSWorkspace.sharedWorkspace.notificationCenter removeObserver:chrome.accessibilityObserver];
    [chrome.history removeFromSuperview];
    [NSEvent removeMonitor:chrome.titlebarMonitor];
}

double bitchord_chrome_top_inset(void *handle) {
    BitChordChrome *chrome = (__bridge BitChordChrome *)handle;
    NSView *view = chrome.content;
    NSWindow *window = view.window;
    if (!window || !(window.styleMask & NSWindowStyleMaskFullSizeContentView)
        || (window.styleMask & NSWindowStyleMaskFullScreen)) return 0;
    NSRect bounds = [view convertRect:view.bounds toView:nil];
    return NSMaxY(bounds) - NSMaxY(window.contentLayoutRect);
}

void bitchord_chrome_set_compact(void *handle, int compact) {
    BitChordChrome *chrome = (__bridge BitChordChrome *)handle;
    NSWindow *window = chrome.content.window;
    NSWindowStyleMask style = window.styleMask;
    window.styleMask = compact ? style & ~NSWindowStyleMaskFullSizeContentView
                               : style | NSWindowStyleMaskFullSizeContentView;
    window.titlebarAppearsTransparent = !compact;
    window.titleVisibility = compact ? NSWindowTitleVisible : NSWindowTitleHidden;
    window.titlebarSeparatorStyle = NSTitlebarSeparatorStyleNone;
}

void bitchord_chrome_appearance(void *handle, int appearance) {
    BitChordChrome *chrome = (__bridge BitChordChrome *)handle;
    NSWindow *window = chrome.content.window;
    NSAppearanceName name = appearance == 1 ? NSAppearanceNameAqua : NSAppearanceNameDarkAqua;
    NSAppearance *next = appearance == 0 ? nil : [NSAppearance appearanceNamed:name];
    if (window.appearance != next) window.appearance = next;
}

unsigned int bitchord_chrome_preferences(void) {
    NSWorkspace *workspace = NSWorkspace.sharedWorkspace;
    NSAppearanceName match = [NSApp.effectiveAppearance
        bestMatchFromAppearancesWithNames:@[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]];
    return (workspace.accessibilityDisplayShouldReduceMotion ? 1 : 0)
        | (workspace.accessibilityDisplayShouldReduceTransparency ? 2 : 0)
        | (workspace.accessibilityDisplayShouldIncreaseContrast ? 4 : 0)
        | ([match isEqualToString:NSAppearanceNameAqua] ? 8 : 0);
}

unsigned int bitchord_chrome_take_history(void *handle) {
    BitChordChrome *chrome = (__bridge BitChordChrome *)handle;
    unsigned int pending = chrome.pending;
    chrome.pending = 0;
    return pending;
}

void bitchord_chrome_history(void *handle, double x, double y, double width, double height,
                            unsigned int enabled) {
    BitChordChrome *chrome = (__bridge BitChordChrome *)handle;
    NSView *content = chrome.content;
    if (width <= 0 || height <= 0) {
        NSResponder *focus = content.window.firstResponder;
        if (focus == chrome.back || focus == chrome.forward) {
            [content.window makeFirstResponder:content];
        }
        chrome.history.hidden = YES;
        return;
    }

    // Rust supplies an egui rect from the actual sidebar layout, in AppKit points.
    NSRect bounds = content.bounds;
    NSRect frame = NSMakeRect(NSMinX(bounds) + x,
        content.isFlipped ? NSMinY(bounds) + y : NSMaxY(bounds) - y - height, width, height);
    if (!NSEqualRects(chrome.history.frame, frame)) {
        chrome.history.frame = frame;
        chrome.historyContents.frame = NSMakeRect(0, 0, width, height);
        if ([chrome.history respondsToSelector:@selector(setCornerRadius:)]) {
            ((NSView<BitChordGlass> *)chrome.history).cornerRadius = height / 2;
        } else {
            chrome.history.layer.cornerRadius = height / 2;
        }
        chrome.back.frame = NSMakeRect(0, 0, width / 2, height);
        chrome.forward.frame = NSMakeRect(width / 2, 0, width / 2, height);
        for (BitChordHistoryButton *button in @[chrome.back, chrome.forward]) {
            button.hoverFill.frame = NSInsetRect(button.frame, 3, 3);
            button.hoverFill.layer.cornerRadius = NSHeight(button.hoverFill.frame) / 2;
        }
    }
    chrome.back.enabled = (enabled & 1) != 0;
    chrome.forward.enabled = (enabled & 2) != 0;
    chrome.history.hidden = NO;
}
