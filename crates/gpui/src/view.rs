use crate::{
    seal::Sealed, AnyElement, AnyModel, AnyWeakModel, AppContext, Bounds, ContentMask, Element,
    ElementId, Entity, EntityId, Flatten, FocusHandle, FocusableView, GlobalElementId, IntoElement,
    LayoutId, Model, ModelContext, PaintIndex, Pixels, PrepaintStateIndex, Render, Style,
    StyleRefinement, TextStyle, VisualContext, WeakModel,
};
use crate::{Empty, Window};
use anyhow::{Context, Result};
use collections::FxHashSet;
use refineable::Refineable;
use std::mem;
use std::{
    any::{type_name, TypeId},
    fmt,
    hash::{Hash, Hasher},
    ops::Range,
};

struct AnyViewState {
    prepaint_range: Range<PrepaintStateIndex>,
    paint_range: Range<PaintIndex>,
    cache_key: ViewCacheKey,
    entities_read: FxHashSet<EntityId>,
}

#[derive(Default)]
struct ViewCacheKey {
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
}

impl<V: Render> Element for Model<V> {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::View(self.entity_id()))
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        window: &mut Window,
        cx: &mut AppContext,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut element = self.update(cx, |view, cx| view.render(window, cx).into_any_element());
        let layout_id = element.request_layout(window, cx);
        (layout_id, element)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut AppContext,
    ) {
        window.set_view_id(self.entity_id());
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut AppContext,
    ) {
        element.paint(window, cx);
    }
}

/// A dynamically-typed handle to a view, which can be downcast to a [View] for a specific type.
#[derive(Clone, Debug)]
pub struct AnyView {
    model: AnyModel,
    render: fn(&AnyView, &mut Window, &mut AppContext) -> AnyElement,
    cached_style: Option<StyleRefinement>,
}

impl<V: Render> From<Model<V>> for AnyView {
    fn from(value: Model<V>) -> Self {
        AnyView {
            model: value.into_any(),
            render: any_view::render::<V>,
            cached_style: None,
        }
    }
}

impl AnyView {
    /// Indicate that this view should be cached when using it as an element.
    /// When using this method, the view's previous layout and paint will be recycled from the previous frame if [ViewContext::notify] has not been called since it was rendered.
    /// The one exception is when [WindowContext::refresh] is called, in which case caching is ignored.
    pub fn cached(mut self, style: StyleRefinement) -> Self {
        self.cached_style = Some(style);
        self
    }

    /// Convert this to a weak handle.
    pub fn downgrade(&self) -> AnyWeakView {
        AnyWeakView {
            model: self.model.downgrade(),
            render: self.render,
        }
    }

    /// Convert this to a [View] of a specific type.
    /// If this handle does not contain a view of the specified type, returns itself in an `Err` variant.
    pub fn downcast<T: 'static>(self) -> Result<Model<T>, Self> {
        match self.model.downcast() {
            Ok(model) => Ok(model),
            Err(model) => Err(Self {
                model,
                render: self.render,
                cached_style: self.cached_style,
            }),
        }
    }

    /// Gets the [TypeId] of the underlying view.
    pub fn entity_type(&self) -> TypeId {
        self.model.entity_type
    }

    /// Gets the entity id of this handle.
    pub fn entity_id(&self) -> EntityId {
        self.model.entity_id()
    }
}

impl PartialEq for AnyView {
    fn eq(&self, other: &Self) -> bool {
        self.model == other.model
    }
}

impl Eq for AnyView {}

impl Element for AnyView {
    type RequestLayoutState = Option<AnyElement>;
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::View(self.entity_id()))
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        window: &mut Window,
        cx: &mut AppContext,
    ) -> (LayoutId, Self::RequestLayoutState) {
        if let Some(style) = self.cached_style.as_ref() {
            let mut root_style = Style::default();
            root_style.refine(style);
            let layout_id = window.request_layout(root_style, None, cx);
            (layout_id, None)
        } else {
            let mut element = (self.render)(self, window, cx);
            let layout_id = element.request_layout(window, cx);
            (layout_id, Some(element))
        }
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut AppContext,
    ) -> Option<AnyElement> {
        window.set_view_id(self.entity_id());
        if self.cached_style.is_some() {
            window.with_element_state::<AnyViewState, _>(
                global_id.unwrap(),
                |element_state, window| {
                    let content_mask = window.content_mask();
                    let text_style = window.text_style();

                    if let Some(mut element_state) = element_state {
                        if element_state.cache_key.bounds == bounds
                            && element_state.cache_key.content_mask == content_mask
                            && element_state.cache_key.text_style == text_style
                            && !window.dirty_views.contains(&self.entity_id())
                            && !window.refreshing
                        {
                            let prepaint_start = window.prepaint_index();
                            window.reuse_prepaint(element_state.prepaint_range.clone());
                            cx.entities.extend_read(&element_state.entities_read);
                            let prepaint_end = window.prepaint_index();
                            element_state.prepaint_range = prepaint_start..prepaint_end;

                            return (None, element_state);
                        }
                    }

                    let refreshing = mem::replace(&mut window.refreshing, true);
                    let prepaint_start = window.prepaint_index();
                    let (mut element, entities_read) =
                        cx.entities_read(|cx| (self.render)(self, window, cx));
                    element.layout_as_root(bounds.size.into(), window, cx);
                    element.prepaint_at(bounds.origin, window, cx);
                    let prepaint_end = window.prepaint_index();
                    window.refreshing = refreshing;

                    (
                        Some(element),
                        AnyViewState {
                            entities_read,
                            prepaint_range: prepaint_start..prepaint_end,
                            paint_range: PaintIndex::default()..PaintIndex::default(),
                            cache_key: ViewCacheKey {
                                bounds,
                                content_mask,
                                text_style,
                            },
                        },
                    )
                },
            )
        } else {
            let mut element = element.take().unwrap();
            element.prepaint(window, cx);
            Some(element)
        }
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        element: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut AppContext,
    ) {
        if self.cached_style.is_some() {
            window.with_element_state::<AnyViewState, _>(
                global_id.unwrap(),
                |element_state, window| {
                    let mut element_state = element_state.unwrap();

                    let paint_start = window.paint_index();

                    if let Some(element) = element {
                        let refreshing = mem::replace(&mut window.refreshing, true);
                        element.paint(window, cx);
                        window.refreshing = refreshing;
                    } else {
                        window.reuse_paint(element_state.paint_range.clone());
                    }

                    let paint_end = window.paint_index();
                    element_state.paint_range = paint_start..paint_end;

                    ((), element_state)
                },
            )
        } else {
            element.as_mut().unwrap().paint(window, cx);
        }
    }
}

impl<V: 'static + Render> IntoElement for Model<V> {
    type Element = Model<V>;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl IntoElement for AnyView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// A weak, dynamically-typed view handle that does not prevent the view from being released.
pub struct AnyWeakView {
    model: AnyWeakModel,
    render: fn(&AnyView, &mut Window, &mut AppContext) -> AnyElement,
}

impl AnyWeakView {
    /// Convert to a strongly-typed handle if the referenced view has not yet been released.
    pub fn upgrade(&self) -> Option<AnyView> {
        let model = self.model.upgrade()?;
        Some(AnyView {
            model,
            render: self.render,
            cached_style: None,
        })
    }
}

impl<V: 'static + Render> From<WeakModel<V>> for AnyWeakView {
    fn from(view: WeakModel<V>) -> Self {
        AnyWeakView {
            model: view.into(),
            render: any_view::render::<V>,
        }
    }
}

impl PartialEq for AnyWeakView {
    fn eq(&self, other: &Self) -> bool {
        self.model == other.model
    }
}

impl std::fmt::Debug for AnyWeakView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyWeakView")
            .field("entity_id", &self.model.entity_id)
            .finish_non_exhaustive()
    }
}

mod any_view {
    use crate::{AnyElement, AnyView, AppContext, IntoElement, Render, Window};

    pub(crate) fn render<V: 'static + Render>(
        view: &AnyView,
        window: &mut Window,
        cx: &mut AppContext,
    ) -> AnyElement {
        let view = view.clone().downcast::<V>().unwrap();
        view.update(cx, |view, cx| view.render(window, cx).into_any_element())
    }
}

/// A view that renders nothing
pub struct EmptyView;

impl Render for EmptyView {
    fn render(&mut self, window: &mut Window, _cx: &mut ModelContext<Self>) -> impl IntoElement {
        Empty
    }
}
