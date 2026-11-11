#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <dlfcn.h>

typedef void (*BCLockCallback)(const void *, unsigned int, double);

enum {
    BCLockWake,
    BCLockToggle,
    BCLockPrevious,
    BCLockNext,
    BCLockSeek,
};

static const CGFloat BCWidth = 252;
static const CGFloat BCHeight = 128;

static CGFloat BCContentTop(NSRect screen) {
    return NSMaxY(screen) - NSHeight(screen) * .26;
}

static NSRect BCLockPanelFrame(NSRect screen) {
    // Leave the clock, password field and Touch ID prompt to loginwindow.
    CGFloat bottom = ceil(NSMinY(screen) + NSHeight(screen) * .24);
    CGFloat top = BCContentTop(screen);
    if (top - bottom < BCHeight || NSWidth(screen) < BCWidth + 32) {
        return NSZeroRect;
    }
    return NSMakeRect(round(NSMidX(screen) - BCWidth / 2),
                      bottom, BCWidth, BCHeight);
}

static NSRect BCExpandedArtworkFrame(NSRect screen, NSRect card, NSSize image) {
    CGFloat bottom = NSMaxY(card) + 12;
    CGFloat maxHeight = MIN(300, BCContentTop(screen) - bottom);
    CGFloat maxWidth = MIN(300, NSWidth(screen) - 32);
    if (NSIsEmptyRect(card) || image.width <= 0 || image.height <= 0
        || MIN(maxHeight, maxWidth) < 112) {
        return NSZeroRect;
    }
    CGFloat scale = MIN(maxWidth / image.width, maxHeight / image.height);
    CGFloat width = floor(image.width * scale);
    CGFloat height = floor(image.height * scale);
    return NSMakeRect(NSMidX(card) - width / 2, bottom, width, height);
}

static BOOL BCLockSessionAllowsPanel(NSDictionary *session) {
    return [session[(__bridge NSString *)kCGSessionOnConsoleKey] isEqual:@YES]
        && [session[(__bridge NSString *)kCGSessionLoginDoneKey] isEqual:@YES]
        && [session[@"CGSSessionScreenIsLocked"] isEqual:@YES];
}

static BOOL BCLockSessionIsLocked(void) {
    NSDictionary *session = CFBridgingRelease(CGSessionCopyCurrentDictionary());
    return BCLockSessionAllowsPanel(session);
}

typedef struct {
    void *library;
    uint32_t (*connectionID)(void);
    uint64_t (*create)(uint32_t, uint32_t, CFDictionaryRef);
    // The macOS 27 implementations enqueue these operations. They do not
    // report a CGError, even though older builds could return one.
    void (*setLevel)(uint32_t, uint64_t, int32_t);
    void (*show)(uint32_t, CFArrayRef);
    void (*hide)(uint32_t, CFArrayRef);
    void (*moveWindow)(uint32_t, uint64_t, CFArrayRef, uint32_t);
    CFArrayRef (*copySpaces)(uint32_t, uint32_t, CFArrayRef);
    void (*destroy)(uint32_t, uint64_t);
} BCSkyLight;

static BCSkyLight BCLoadSkyLight(void) {
    // Space ABI and level: https://github.com/Lakr233/SkyLightWindow
    void *lib = dlopen("/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/SkyLight",
                       RTLD_LAZY | RTLD_LOCAL);
    if (!lib) return (BCSkyLight){0};
    BCSkyLight api = {
        .library = lib,
        .connectionID = dlsym(lib, "SLSMainConnectionID"),
        .create = dlsym(lib, "SLSSpaceCreate"),
        .setLevel = dlsym(lib, "SLSSpaceSetAbsoluteLevel"),
        .show = dlsym(lib, "SLSShowSpaces"),
        .hide = dlsym(lib, "SLSHideSpaces"),
        .moveWindow = dlsym(lib, "SLSSpaceAddWindowsAndRemoveFromSpaces"),
        .copySpaces = dlsym(lib, "SLSCopySpacesForWindows"),
        .destroy = dlsym(lib, "SLSSpaceDestroy"),
    };
    if (api.connectionID && api.create && api.setLevel && api.show && api.hide
        && api.moveWindow && api.copySpaces && api.destroy) {
        return api;
    }
    dlclose(lib);
    return (BCSkyLight){0};
}

@interface BCLockPanel : NSPanel
@end
@implementation BCLockPanel
- (BOOL)canBecomeKey { return NO; }
- (BOOL)canBecomeMain { return NO; }
- (void)makeKeyWindow {}
@end

@interface BCLockButton : NSButton
@end
@implementation BCLockButton
- (BOOL)acceptsFirstMouse:(__unused NSEvent *)event { return YES; }
@end

@interface BCLockArtworkButton : BCLockButton
@end
@implementation BCLockArtworkButton
- (void)resetCursorRects {
    [super resetCursorRects];
    if (self.enabled) [self addCursorRect:self.bounds cursor:NSCursor.pointingHandCursor];
}
@end

@interface BCLockSliderCell : NSSliderCell
@end
@implementation BCLockSliderCell
- (void)drawBarInside:(NSRect)rect flipped:(__unused BOOL)flipped {
    NSRect track = NSMakeRect(NSMinX(rect), NSMidY(rect) - 2, NSWidth(rect), 4);
    NSBezierPath *outline = [NSBezierPath bezierPathWithRoundedRect:track xRadius:2 yRadius:2];
    [[NSColor colorWithWhite:1 alpha:.21] setFill];
    [outline fill];
    CGFloat fraction = self.doubleValue / MAX(self.maxValue, 1);
    if (fraction > 0) {
        [NSGraphicsContext saveGraphicsState];
        [outline addClip];
        track.size.width *= MIN(fraction, 1);
        [[NSColor colorWithWhite:1 alpha:.88] setFill];
        NSRectFillUsingOperation(track, NSCompositingOperationSourceOver);
        [NSGraphicsContext restoreGraphicsState];
    }
}

- (void)drawKnob:(NSRect)rect {
    if (!self.highlighted) return;
    [[NSColor colorWithWhite:1 alpha:.94] setFill];
    NSRect dot = NSMakeRect(NSMidX(rect) - 3, NSMidY(rect) - 3, 6, 6);
    [[NSBezierPath bezierPathWithOvalInRect:dot] fill];
}
@end

@interface BCLockSlider : NSSlider
@property(nonatomic, readonly) BOOL tracking;
@end
@implementation BCLockSlider
- (BOOL)acceptsFirstMouse:(__unused NSEvent *)event { return YES; }
- (void)mouseDown:(NSEvent *)event {
    _tracking = YES;
    [super mouseDown:event];
    _tracking = NO;
}
@end

static NSTextField *BCLabel(NSView *parent, NSRect frame, NSFont *font, NSColor *color) {
    NSTextField *label = [NSTextField labelWithString:@""];
    label.frame = frame;
    label.font = font;
    label.textColor = color;
    label.lineBreakMode = NSLineBreakByTruncatingTail;
    [parent addSubview:label];
    return label;
}

static NSImage *BCSymbol(NSString *name, NSString *label, CGFloat size) {
    return [[NSImage imageWithSystemSymbolName:name accessibilityDescription:label]
        imageWithSymbolConfiguration:[NSImageSymbolConfiguration configurationWithPointSize:size
                                                                                    weight:NSFontWeightSemibold]];
}

static NSString *BCTime(double seconds) {
    NSInteger whole = (NSInteger)seconds;
    return [NSString stringWithFormat:@"%ld:%02ld", (long)(whole / 60), (long)(whole % 60)];
}

static BCLockPanel *BCMakePanel(NSRect frame) {
    BCLockPanel *window = [[BCLockPanel alloc] initWithContentRect:frame
        styleMask:NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel
        backing:NSBackingStoreBuffered defer:NO];
    window.opaque = NO;
    window.backgroundColor = NSColor.clearColor;
    window.hasShadow = YES;
    window.movable = NO;
    window.releasedWhenClosed = NO;
    window.hidesOnDeactivate = NO;
    window.becomesKeyOnlyIfNeeded = YES;
    window.canBecomeVisibleWithoutLogin = YES;
    window.level = CGShieldingWindowLevel() + 1;
    window.collectionBehavior = NSWindowCollectionBehaviorStationary | NSWindowCollectionBehaviorIgnoresCycle;
    window.alphaValue = 0;
    window.ignoresMouseEvents = YES;
    return window;
}

static NSView *BCPlayerSurface(NSView *content) {
    const CGFloat radius = 19;
#if __MAC_OS_X_VERSION_MAX_ALLOWED >= 260000
    if (@available(macOS 26.0, *)) {
        NSGlassEffectView *glass = [[NSGlassEffectView alloc] initWithFrame:content.frame];
        glass.style = NSGlassEffectViewStyleClear;
        glass.cornerRadius = radius;
        glass.tintColor = [NSColor colorWithWhite:.12 alpha:.42];
        glass.contentView = content;
        return glass;
    }
#endif
    NSVisualEffectView *blur = [[NSVisualEffectView alloc] initWithFrame:content.frame];
    blur.material = NSVisualEffectMaterialHUDWindow;
    blur.blendingMode = NSVisualEffectBlendingModeBehindWindow;
    blur.state = NSVisualEffectStateActive;
    blur.wantsLayer = YES;
    blur.layer.cornerRadius = radius;
    blur.layer.masksToBounds = YES;
    [blur addSubview:content];
    return blur;
}

@interface BCLockScreen : NSObject {
@public
    BCSkyLight api;
    BCLockCallback callback;
    const void *context;
    NSString *title;
    NSString *artist;
    NSImage *cover;
    double position;
    double duration;
    BOOL playing;
    BOOL canSeek;
    BOOL failed;
@private
    BCLockPanel *panel;
    BCLockArtworkButton *thumbnail;
    BCLockPanel *artworkPanel;
    NSRect screenFrame;
    NSInteger artworkPlacementFailures;
    NSTextField *titleLabel;
    NSTextField *artistLabel;
    NSTextField *elapsedLabel;
    NSTextField *durationLabel;
    BCLockButton *playButton;
    BCLockSlider *slider;
    NSMutableArray *observers;
    NSTimer *timer;
    BOOL asleep;
    NSInteger repairTicks;
    NSInteger ticks;
    NSInteger presentationFailures;
    uint32_t connection;
    uint64_t space;
}
- (void)start;
- (void)stop;
- (void)collapseArtwork;
- (void)placeArtwork;
- (void)refreshMetadata;
- (void)refresh;
- (void)reconcile:(BOOL)repair;
@end

@implementation BCLockScreen

- (void)watch:(NSNotificationCenter *)center
          name:(NSNotificationName)name
        handle:(void (^)(BCLockScreen *))handle {
    __weak BCLockScreen *weakSelf = self;
    id token = [center addObserverForName:name object:nil queue:NSOperationQueue.mainQueue
                             usingBlock:^(__unused NSNotification *note) {
        BCLockScreen *owner = weakSelf;
        if (owner) handle(owner);
    }];
    [observers addObject:@[center, token]];
}

- (void)repair {
    // loginwindow may rebuild its visible Spaces just after the notification.
    repairTicks = 2;
    [self reconcile:YES];
}

- (void)start {
    if (timer || failed) return;
    asleep = CGDisplayIsAsleep(CGMainDisplayID()) != 0;
    observers = [NSMutableArray array];
    NSNotificationCenter *distributed = NSDistributedNotificationCenter.defaultCenter;
    [self watch:distributed name:@"com.apple.screenIsLocked" handle:^(BCLockScreen *s) {
        [s repair];
    }];
    [self watch:distributed name:@"com.apple.screenIsUnlocked" handle:^(BCLockScreen *s) {
        [s dismiss];
    }];
    NSNotificationCenter *workspace = NSWorkspace.sharedWorkspace.notificationCenter;
    [self watch:workspace name:NSWorkspaceScreensDidSleepNotification handle:^(BCLockScreen *s) {
        s->asleep = YES;
        [s dismiss];
    }];
    [self watch:workspace name:NSWorkspaceScreensDidWakeNotification handle:^(BCLockScreen *s) {
        s->asleep = NO;
        [s repair];
    }];
    [self watch:workspace name:NSWorkspaceSessionDidResignActiveNotification handle:^(BCLockScreen *s) {
        [s dismiss];
    }];
    [self watch:workspace name:NSWorkspaceSessionDidBecomeActiveNotification handle:^(BCLockScreen *s) {
        [s repair];
    }];
    [self watch:NSNotificationCenter.defaultCenter name:NSApplicationDidChangeScreenParametersNotification
          handle:^(BCLockScreen *s) { [s repair]; }];

    __weak BCLockScreen *weakSelf = self;
    timer = [NSTimer timerWithTimeInterval:1 repeats:YES block:^(__unused NSTimer *tick) {
        BCLockScreen *s = weakSelf;
        if (!s) return;
        // Polls recover a wake or session change whose notification was missed.
        s->asleep = CGDisplayIsAsleep(CGMainDisplayID()) != 0;
        [s reconcile:s->repairTicks > 0 || ++s->ticks % 5 == 0];
        if (s->repairTicks > 0) s->repairTicks--;
        if (s->panel.alphaValue > 0 && s->playing) s->callback(s->context, BCLockWake, 0);
    }];
    timer.tolerance = .1;
    [NSRunLoop.mainRunLoop addTimer:timer forMode:NSRunLoopCommonModes];
    [self repair];
}

- (void)stop {
    [timer invalidate];
    timer = nil;
    for (NSArray *entry in observers) {
        [entry[0] removeObserver:entry[1]];
    }
    observers = nil;
    asleep = NO;
    presentationFailures = 0;
    [self dismiss];
}

- (void)dismiss {
    [self collapseArtwork];
    [panel orderOut:nil];
    [panel close];
    panel = nil;
    if (space) {
        api.hide(connection, (__bridge CFArrayRef)@[@(space)]);
        api.destroy(connection, space);
        space = 0;
    }
    thumbnail = nil;
    titleLabel = nil;
    artistLabel = nil;
    elapsedLabel = nil;
    durationLabel = nil;
    playButton = nil;
    slider = nil;
}

- (BOOL)acceptsCommands {
    return panel.alphaValue > 0 && !asleep && BCLockSessionIsLocked();
}

- (void)transport:(NSButton *)sender {
    if ([self acceptsCommands]) callback(context, (unsigned int)sender.tag, 0);
}

- (void)seek:(NSSlider *)sender {
    if (canSeek && [self acceptsCommands]) callback(context, BCLockSeek, sender.doubleValue);
}

- (BCLockArtworkButton *)artworkButton:(NSRect)frame radius:(CGFloat)radius {
    BCLockArtworkButton *button = [[BCLockArtworkButton alloc] initWithFrame:frame];
    button.bordered = NO;
    button.imagePosition = NSImageOnly;
    ((NSButtonCell *)button.cell).imageScaling = NSImageScaleProportionallyUpOrDown;
    button.wantsLayer = YES;
    button.layer.cornerRadius = radius;
    button.layer.masksToBounds = YES;
    button.layer.backgroundColor = [NSColor colorWithWhite:1 alpha:.1].CGColor;
    button.target = self;
    button.action = @selector(toggleArtwork:);
    return button;
}

- (void)updateArtworkButton {
    NSString *label = artworkPanel ? @"Collapse album artwork" : @"Enlarge album artwork";
    thumbnail.toolTip = thumbnail.enabled ? label : nil;
    [thumbnail setAccessibilityLabel:thumbnail.enabled ? label : @"Album artwork"];
}

- (void)collapseArtwork {
    [artworkPanel orderOut:nil];
    [artworkPanel close];
    artworkPanel = nil;
    artworkPlacementFailures = 0;
    [self updateArtworkButton];
}

- (void)toggleArtwork:(__unused NSButton *)sender {
    if (![self acceptsCommands] || !cover) return;
    if (artworkPanel) {
        [self collapseArtwork];
        return;
    }
    NSRect frame = BCExpandedArtworkFrame(screenFrame, panel.frame, cover.size);
    if (NSIsEmptyRect(frame)) return;

    artworkPanel = BCMakePanel(frame);
    NSRect bounds = NSMakeRect(0, 0, NSWidth(frame), NSHeight(frame));
    BCLockArtworkButton *image = [self artworkButton:bounds radius:14];
    image.image = cover;
    image.toolTip = @"Collapse album artwork";
    [image setAccessibilityLabel:image.toolTip];
    image.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    artworkPanel.contentView = image;
    [self updateArtworkButton];
    [self placeArtwork];
}

- (BCLockButton *)button:(NSView *)parent
                     x:(CGFloat)x
                symbol:(NSString *)symbol
                 label:(NSString *)label
                action:(NSInteger)action {
    BCLockButton *button = [[BCLockButton alloc] initWithFrame:NSMakeRect(x, 7, 40, 36)];
    button.bordered = NO;
    button.image = BCSymbol(symbol, label, action == BCLockToggle ? 19 : 15);
    button.imagePosition = NSImageOnly;
    button.contentTintColor = NSColor.whiteColor;
    button.target = self;
    button.action = @selector(transport:);
    button.tag = action;
    button.toolTip = label;
    [button setAccessibilityLabel:label];
    [parent addSubview:button];
    return button;
}

- (void)buildPanel:(NSRect)frame {
    panel = BCMakePanel(frame);
    NSView *card = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, BCWidth, BCHeight)];
    panel.contentView = BCPlayerSurface(card);
    panel.contentView.appearance = [NSAppearance appearanceNamed:NSAppearanceNameDarkAqua];

    thumbnail = [self artworkButton:NSMakeRect(12, 72, 44, 44) radius:8];
    thumbnail.contentTintColor = [NSColor colorWithWhite:1 alpha:.8];
    [card addSubview:thumbnail];

    NSColor *secondary = [NSColor colorWithWhite:1 alpha:.68];
    titleLabel = BCLabel(card, NSMakeRect(66, 94, 174, 19),
                         [NSFont systemFontOfSize:13 weight:NSFontWeightSemibold], NSColor.whiteColor);
    artistLabel = BCLabel(card, NSMakeRect(66, 76, 174, 16), [NSFont systemFontOfSize:11], secondary);
    NSFont *digits = [NSFont monospacedDigitSystemFontOfSize:10 weight:NSFontWeightMedium];
    elapsedLabel = BCLabel(card, NSMakeRect(12, 50, 35, 14), digits, secondary);
    durationLabel = BCLabel(card, NSMakeRect(205, 50, 35, 14), digits, secondary);
    durationLabel.alignment = NSTextAlignmentRight;

    slider = [[BCLockSlider alloc] initWithFrame:NSMakeRect(47, 47, 158, 20)];
    slider.cell = [[BCLockSliderCell alloc] init];
    slider.controlSize = NSControlSizeSmall;
    slider.continuous = NO;
    slider.target = self;
    slider.action = @selector(seek:);
    [slider setAccessibilityLabel:@"Playback position"];
    [card addSubview:slider];

    [self button:card x:62 symbol:@"backward.fill" label:@"Previous track" action:BCLockPrevious];
    playButton = [self button:card x:106 symbol:@"play.fill" label:@"Play" action:BCLockToggle];
    [self button:card x:150 symbol:@"forward.fill" label:@"Next track" action:BCLockNext];
    [self refreshMetadata];
    [self refresh];
}

- (void)refreshMetadata {
    if (!panel) return;
    titleLabel.stringValue = title ?: @"";
    artistLabel.stringValue = artist ?: @"";
    thumbnail.image = cover ?: BCSymbol(@"music.note", @"Album artwork", 17);
    thumbnail.enabled = cover && !NSIsEmptyRect(BCExpandedArtworkFrame(screenFrame, panel.frame, cover.size));
    [self updateArtworkButton];
}

- (void)refresh {
    if (!panel) return;
    elapsedLabel.stringValue = BCTime(position);
    durationLabel.stringValue = duration > 0 ? BCTime(duration) : @"–:––";
    if (!slider.tracking) {
        slider.maxValue = duration > 0 ? duration : 1;
        slider.doubleValue = MIN(position, slider.maxValue);
        slider.enabled = canSeek;
    }
    NSString *label = playing ? @"Pause" : @"Play";
    if (![playButton.toolTip isEqualToString:label]) {
        playButton.image = BCSymbol(playing ? @"pause.fill" : @"play.fill", label, 19);
        playButton.toolTip = label;
        [playButton setAccessibilityLabel:label];
    }
}

- (BOOL)adoptPanel:(BCLockPanel *)window {
    [window orderFrontRegardless];
    NSInteger number = window.windowNumber;
    if (number <= 0) return NO;
    NSArray *windows = @[@((uint32_t)number)];
    const uint32_t allSpaces = 7;
    api.moveWindow(connection, space, (__bridge CFArrayRef)windows, allSpaces);
    NSArray *spaces = CFBridgingRelease(api.copySpaces(connection, allSpaces, (__bridge CFArrayRef)windows));
    return [spaces containsObject:@(space)];
}

- (void)placeArtwork {
    if (!artworkPanel) return;
    NSRect frame = BCExpandedArtworkFrame(screenFrame, panel.frame, cover.size);
    if (NSIsEmptyRect(frame)) {
        [self collapseArtwork];
        return;
    }
    if (!NSEqualRects(artworkPanel.frame, frame)) {
        [artworkPanel setFrame:frame display:YES];
    }
    BOOL attached = [self adoptPanel:artworkPanel];
    if (!BCLockSessionIsLocked()) {
        [self dismiss];
    } else if (attached) {
        artworkPlacementFailures = 0;
        artworkPanel.alphaValue = 1;
        artworkPanel.ignoresMouseEvents = NO;
    } else {
        artworkPanel.alphaValue = 0;
        artworkPanel.ignoresMouseEvents = YES;
        if (++artworkPlacementFailures >= 3) [self collapseArtwork];
    }
}

- (void)reconcile:(BOOL)repair {
    if (failed || !title || asleep || !BCLockSessionIsLocked()) {
        if (panel) [self dismiss];
        return;
    }
    if (panel.alphaValue > 0 && !repair && (!artworkPanel || artworkPanel.alphaValue > 0)) return;

    NSScreen *screen = NSScreen.screens.firstObject;
    NSRect frame = screen ? BCLockPanelFrame(screen.frame) : NSZeroRect;
    if (NSIsEmptyRect(frame)) {
        [self dismiss];
        return;
    }

    BOOL screenChanged = !NSEqualRects(screenFrame, screen.frame);
    screenFrame = screen.frame;
    if (!panel) {
        [self buildPanel:frame];
        connection = api.connectionID();
        space = connection ? api.create(connection, 1, NULL) : 0;
        if (!space) {
            [self dismiss];
            [self fail];
            return;
        }
        // 400 is the WindowServer level used by Notification Center while locked.
        api.setLevel(connection, space, 400);
    } else if (!NSEqualRects(panel.frame, frame)) {
        [panel setFrame:frame display:YES];
    }
    if (screenChanged) [self refreshMetadata];
    api.show(connection, (__bridge CFArrayRef)@[@(space)]);
    BOOL attached = [self adoptPanel:panel];
    if (!BCLockSessionIsLocked()) {
        [self dismiss];
    } else if (attached) {
        presentationFailures = 0;
        panel.alphaValue = 1;
        panel.ignoresMouseEvents = NO;
        [self placeArtwork];
    } else {
        // WindowServer can adopt the panel asynchronously. Keep it invisible
        // and let the next tick retry before giving up.
        panel.alphaValue = 0;
        panel.ignoresMouseEvents = YES;
        artworkPanel.alphaValue = 0;
        artworkPanel.ignoresMouseEvents = YES;
        [self fail];
    }
}

- (void)fail {
    if (!BCLockSessionIsLocked() || ++presentationFailures < 3) return;
    NSLog(@"BitChord: could not present Lock Screen Player through SkyLight");
    failed = YES;
    [self stop];
    callback(context, BCLockWake, 0);
}

@end

void *bitchord_lock_create(BCLockCallback callback, const void *context) {
    BCSkyLight api = BCLoadSkyLight();
    if (!api.library) return NULL;
    BCLockScreen *player = [BCLockScreen new];
    player->api = api;
    player->callback = callback;
    player->context = context;
    return (__bridge_retained void *)player;
}

void bitchord_lock_destroy(void *handle) {
    BCLockScreen *player = CFBridgingRelease(handle);
    [player stop];
    dlclose(player->api.library);
}

void bitchord_lock_track(void *handle, const uint8_t *title, size_t titleLength,
                         const uint8_t *artist, size_t artistLength) {
    BCLockScreen *player = (__bridge BCLockScreen *)handle;
    [player collapseArtwork];
    player->title = title ? [[NSString alloc] initWithBytes:title length:titleLength encoding:NSUTF8StringEncoding] : nil;
    player->artist = artist ? [[NSString alloc] initWithBytes:artist length:artistLength encoding:NSUTF8StringEncoding] : nil;
    player->cover = nil;
    if (title) {
        [player start];
        [player refreshMetadata];
    } else {
        [player stop];
    }
}

void bitchord_lock_artwork(void *handle, const uint8_t *pixels, size_t width, size_t height) {
    BCLockScreen *player = (__bridge BCLockScreen *)handle;
    NSData *data = [NSData dataWithBytes:pixels length:width * height * 4];
    CGDataProviderRef provider = CGDataProviderCreateWithCFData((__bridge CFDataRef)data);
    CGColorSpaceRef colorSpace = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    if (provider && colorSpace) {
        CGImageRef image = CGImageCreate(width, height, 8, 32, width * 4, colorSpace,
                                         kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big,
                                         provider, NULL, YES, kCGRenderingIntentDefault);
        if (image) {
            player->cover = [[NSImage alloc] initWithCGImage:image size:NSZeroSize];
            CGImageRelease(image);
        }
    }
    if (colorSpace) CGColorSpaceRelease(colorSpace);
    if (provider) CGDataProviderRelease(provider);
    [player refreshMetadata];
}

bool bitchord_lock_update(void *handle, double position, double duration, bool playing, bool canSeek) {
    BCLockScreen *player = (__bridge BCLockScreen *)handle;
    player->position = position;
    player->duration = duration;
    player->playing = playing;
    player->canSeek = canSeek;
    [player refresh];
    return !player->failed;
}
