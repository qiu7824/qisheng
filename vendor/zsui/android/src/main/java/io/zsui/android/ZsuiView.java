package io.zsui.android;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Bitmap;
import android.graphics.Rect;
import android.graphics.LinearGradient;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.RectF;
import android.graphics.Shader;
import android.graphics.Typeface;
import android.text.Layout;
import android.text.StaticLayout;
import android.text.TextPaint;
import android.text.TextUtils;
import android.util.Log;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewConfiguration;
import android.content.res.Configuration;
import android.util.LruCache;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Iterator;
import java.util.Map;
import java.util.LinkedHashMap;
import java.util.Objects;

/** Android Canvas host for ZSUI's retained Rust View and typed input route. */
public final class ZsuiView extends View {
    public interface Driver { JSONObject dispatch(JSONObject event) throws Exception; }
    private final Driver driver;
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final TextPaint textPaint = new TextPaint(Paint.ANTI_ALIAS_FLAG);
    private final Path path = new Path();
    private JSONObject frame;
    private final LinkedHashMap<String, Bitmap> images = new LinkedHashMap<>(32,.75f,true);
    private final LruCache<TextLayoutKey,StaticLayout> textLayouts=new LruCache<>(256);
    private int activePointer = -1;
    private float downX, downY, lastY;
    private boolean dragging;
    private boolean horizontal;
    private final int slop;
    private final ZsuiKineticScroller kinetic;
    private final android.graphics.drawable.RippleDrawable touchFeedback=new android.graphics.drawable.RippleDrawable(android.content.res.ColorStateList.valueOf(0x22ffffff),null,null);
    private final Runnable clearTouchFeedback=this::stopTouchFeedback;
    public void setTouchFeedbackColor(int color){touchFeedback.setColor(android.content.res.ColorStateList.valueOf(color));}
    @Override protected boolean verifyDrawable(android.graphics.drawable.Drawable drawable){return drawable==touchFeedback||super.verifyDrawable(drawable);}
    private void stopTouchFeedback(){removeCallbacks(clearTouchFeedback);touchFeedback.setState(new int[0]);touchFeedback.setVisible(false,false);touchFeedback.jumpToCurrentState();invalidate();}
    /** Scene changes are application-owned; interpolation and lifetime belong to this View. */
    public enum SceneMotion { FADE, SLIDE_LEFT, SLIDE_RIGHT }
    private String sceneKey;
    private SceneMotion sceneMotion=SceneMotion.FADE;
    private Bitmap previousScene;
    private android.animation.ValueAnimator sceneAnimator;
    private float sceneProgress=1f;
    private boolean transitionTouch;

    /** Call from the Driver before returning a replacement frame. Repeated keys do not animate. */
    public void setSceneTransition(String key, SceneMotion motion) {
        if(android.os.Looper.myLooper()!=android.os.Looper.getMainLooper())throw new IllegalStateException("Scene changes require the UI thread");
        if(Objects.equals(sceneKey,key))return;
        String previousKey=sceneKey;sceneKey=key;
        finishSceneTransition();
        if(previousKey==null || frame==null || getWidth()<=0 || getHeight()<=0 || !hasWindowFocus() || !android.animation.ValueAnimator.areAnimatorsEnabled())return;
        float scale=Math.min(1f,(float)Math.sqrt((4f*1024*1024)/(getWidth()*(double)getHeight()*4)));
        int width=Math.max(1,(int)(getWidth()*scale)),height=Math.max(1,(int)(getHeight()*scale));
        try {
            previousScene=Bitmap.createBitmap(width,height,Bitmap.Config.ARGB_8888);
            Canvas snapshot=new Canvas(previousScene);snapshot.scale(width/(float)getWidth(),height/(float)getHeight());drawFrame(snapshot);
        } catch(OutOfMemoryError unavailable) {finishSceneTransition();return;}
        sceneMotion=motion==null?SceneMotion.FADE:motion;
        sceneProgress=0;
        sceneAnimator=android.animation.ValueAnimator.ofFloat(0,1);
        sceneAnimator.setDuration(220);
        sceneAnimator.setInterpolator(new android.view.animation.DecelerateInterpolator(1.5f));
        sceneAnimator.addUpdateListener(value->{sceneProgress=(float)value.getAnimatedValue();invalidate();});
        sceneAnimator.addListener(new android.animation.AnimatorListenerAdapter(){@Override public void onAnimationEnd(android.animation.Animator value){finishSceneTransition();invalidate();}});
        sceneAnimator.start();
    }
    private void finishSceneTransition(){
        if(sceneAnimator!=null){android.animation.ValueAnimator old=sceneAnimator;sceneAnimator=null;old.removeAllListeners();old.removeAllUpdateListeners();old.cancel();}
        sceneProgress=1f;
        if(previousScene!=null){previousScene.recycle();previousScene=null;}
    }


    public ZsuiView(Context context, Driver driver) {
        super(context);
        this.driver = driver;
        kinetic = new ZsuiKineticScroller(this);
        touchFeedback.setCallback(this);touchFeedback.setRadius(Math.round(28*getResources().getDisplayMetrics().density));
        slop = ViewConfiguration.get(context).getScaledTouchSlop();
        setFocusable(true);
        setFocusableInTouchMode(true);
        setContentDescription("应用内容");
        setImportantForAccessibility(IMPORTANT_FOR_ACCESSIBILITY_YES);
    }

    public void submit(JSONObject event) {
        if (android.os.Looper.myLooper() != android.os.Looper.getMainLooper()) {
            post(() -> submit(event));
            return;
        }
        try {
            event.put("raster_encoding","argb_base64");
            event.put("retained_images", new JSONArray(images.keySet()));
            JSONObject response = driver.dispatch(event);
            if (response.has("frame") && !response.isNull("frame")) {
                frame = response.getJSONObject("frame");
                if (!"zsui.android.frame/v1".equals(frame.getString("schema"))) {
                    throw new IllegalStateException("Unsupported ZSUI frame schema");
                }
                JSONArray errors = frame.optJSONArray("errors");
                if (errors != null && errors.length() > 0) Log.e("ZSUI", errors.toString());
                retainImages();
                kinetic.apply(frame.optJSONObject("interactions"));
                invalidate();
            }
        } catch (Exception error) {
            Log.e("ZSUI", "Android host dispatch failed", error);
        }
    }

    public static JSONObject event(String type, Object... pairs) {
        JSONObject value = new JSONObject();
        try {
            value.put("type", type);
            for (int index = 0; index < pairs.length; index += 2) value.put((String)pairs[index], pairs[index + 1]);
        } catch (JSONException error) { throw new IllegalArgumentException(error); }
        return value;
    }

    public void refresh() { submit(event("refresh")); }

    public void setResumed(boolean resumed) {
        kinetic.stop();
        finishSceneTransition();stopTouchFeedback();transitionTouch=false;
        activePointer = -1;
        dragging = false;
        submit(event("lifecycle", "resumed", resumed));
        if (resumed && getWidth() > 0 && getHeight() > 0) resize(getWidth(), getHeight());
    }

    @Override protected void onSizeChanged(int width, int height, int oldWidth, int oldHeight) {
        kinetic.stop();finishSceneTransition();stopTouchFeedback();transitionTouch=false;activePointer=-1;dragging=false;horizontal=false;
        touchFeedback.setBounds(0,0,width,height);
        if (width > 0 && height > 0) resize(width, height);
    }

    private void resize(int width, int height) {
        submit(event("resize", "width", width, "height", height,
            "density", getResources().getDisplayMetrics().density,
            "font_scale", getResources().getConfiguration().fontScale));
    }

    private void pointer(String phase, float x, float y) {
        submit(event("touch", "phase", phase, "pointer", activePointer, "x", x, "y", y));
    }

    @Override public boolean onTouchEvent(MotionEvent event) {
        int action = event.getActionMasked();
        if(action==MotionEvent.ACTION_DOWN && sceneAnimator!=null){finishSceneTransition();transitionTouch=true;invalidate();}
        if(transitionTouch){if(action==MotionEvent.ACTION_UP||action==MotionEvent.ACTION_CANCEL)transitionTouch=false;return true;}
        if(kinetic.observe(event)){
            stopTouchFeedback();
            if(activePointer!=-1)pointer("cancel",event.getX(),event.getY());
            activePointer=-1;dragging=false;horizontal=false;return true;
        }
        if (action == MotionEvent.ACTION_DOWN) {
            removeCallbacks(clearTouchFeedback);
            int radius=touchFeedback.getRadius(),x=Math.round(event.getX()),y=Math.round(event.getY());
            touchFeedback.setHotspotBounds(x-radius,y-radius,x+radius,y+radius);
            touchFeedback.setVisible(true,false);touchFeedback.setHotspot(event.getX(),event.getY());touchFeedback.setState(new int[]{android.R.attr.state_enabled,android.R.attr.state_pressed});
            requestFocus(); activePointer = event.getPointerId(0);
            downX = event.getX(); downY = lastY = event.getY(); dragging = false; horizontal = false;
            pointer("down", downX, downY);
            return true;
        }
        if (action == MotionEvent.ACTION_CANCEL || action == MotionEvent.ACTION_POINTER_DOWN ||
            (action == MotionEvent.ACTION_POINTER_UP && event.getPointerId(event.getActionIndex()) == activePointer)) {
            stopTouchFeedback();pointer("cancel", downX, lastY); activePointer = -1; dragging = false;
            return true;
        }
        if (activePointer == -1) return true;
        int index = event.findPointerIndex(activePointer);
        if (index < 0) { stopTouchFeedback();pointer("cancel", downX, lastY); activePointer = -1; return true; }
        float x = event.getX(index), y = event.getY(index);
        if (action == MotionEvent.ACTION_MOVE) {
            if(Math.abs(x-downX)>slop||Math.abs(y-downY)>slop)stopTouchFeedback();
            if (!dragging && !horizontal && Math.abs(x - downX) > slop && Math.abs(x - downX) > Math.abs(y - downY)) horizontal = true;
            if (!dragging && !horizontal && Math.abs(y - downY) > slop) {
                pointer("cancel", x, y); dragging = true;
            }
            if (dragging) submit(event("scroll", "x", downX, "y", downY, "delta_y", lastY - y));
            else pointer("move", x, y);
            lastY = y;
        } else if (action == MotionEvent.ACTION_UP) {
            touchFeedback.setState(new int[]{android.R.attr.state_enabled});
            postDelayed(clearTouchFeedback,300);
            if (!dragging) { pointer("up", x, y); if(!horizontal)performClick(); }
            activePointer = -1; dragging = false;
        }
        return true;
    }

    @Override public boolean performClick() { super.performClick(); return true; }

    @Override public void onWindowFocusChanged(boolean focused){
        super.onWindowFocusChanged(focused);
        if(!focused){stopTouchFeedback();finishSceneTransition();transitionTouch=false;kinetic.stop();if(activePointer!=-1)pointer("cancel",downX,lastY);activePointer=-1;dragging=false;horizontal=false;}
    }
    @Override protected void onDetachedFromWindow(){stopTouchFeedback();touchFeedback.setVisible(false,false);finishSceneTransition();kinetic.stop();super.onDetachedFromWindow();}
    @Override protected void onConfigurationChanged(Configuration configuration){super.onConfigurationChanged(configuration);textLayouts.evictAll();}

    @Override protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        if (frame == null) return;
        if(previousScene==null){drawFrame(canvas);touchFeedback.draw(canvas);return;}
        int base=canvas.save();
        canvas.clipRect(0,0,getWidth(),getHeight());
        float direction=sceneMotion==SceneMotion.SLIDE_RIGHT?-1f:1f;
        Paint layer=new Paint(Paint.ANTI_ALIAS_FLAG|Paint.FILTER_BITMAP_FLAG);
        int old=canvas.save();
        if(sceneMotion!=SceneMotion.FADE)canvas.translate(-direction*getWidth()*.25f*sceneProgress,0);
        layer.setAlpha(Math.round(255*(1-sceneProgress)));
        canvas.drawBitmap(previousScene,null,new RectF(0,0,getWidth(),getHeight()),layer);
        canvas.restoreToCount(old);
        int incoming=canvas.saveLayerAlpha(0,0,getWidth(),getHeight(),Math.round(255*sceneProgress));
        if(sceneMotion!=SceneMotion.FADE)canvas.translate(direction*getWidth()*.18f*(1-sceneProgress),0);
        drawFrame(canvas);
        canvas.restoreToCount(incoming);canvas.restoreToCount(base);
    }

    private void drawFrame(Canvas canvas) {
        if(frame==null)return;
        int base = canvas.save();
        int clipDepth = 0;
        try {
            canvas.drawColor((int)frame.getLong("background"));
            JSONArray commands = frame.getJSONArray("commands");
            for (int index = 0; index < commands.length(); index++) {
                JSONObject item = commands.getJSONObject(index);
                String op = item.getString("op");
                paint.reset(); paint.setAntiAlias(true); paint.setStyle(Paint.Style.FILL);
                paint.setColor((int)item.optLong("color", 0xff000000L));
                RectF rect = item.has("rect") ? rectangle(item.getJSONObject("rect")) : null;
                switch (op) {
                    case "rect": canvas.drawRect(rect, paint); break;
                    case "stroke_rect":
                        paint.setStyle(Paint.Style.STROKE); paint.setStrokeWidth((float)item.getDouble("width"));
                        canvas.drawRect(rect, paint); break;
                    case "arc":
                        paint.setStyle(Paint.Style.STROKE); paint.setStrokeWidth((float)item.getDouble("width"));
                        canvas.drawArc(rect, (float)item.getDouble("start"), (float)item.getDouble("sweep"), false, paint); break;
                    case "round":
                        float radius = (float)item.getDouble("radius");
                        canvas.drawRoundRect(rect, radius, radius, paint);
                        if (item.has("stroke") && !item.isNull("stroke")) {
                            paint.setColor((int)item.getLong("stroke")); paint.setStrokeWidth(1); paint.setStyle(Paint.Style.STROKE);
                            canvas.drawRoundRect(rect, radius, radius, paint);
                        }
                        break;
                    case "polygon": case "polyline":
                        path.reset(); JSONArray points = item.getJSONArray("points");
                        for (int p = 0; p < points.length(); p++) {
                            JSONObject point = points.getJSONObject(p);
                            if (p == 0) path.moveTo((float)point.getDouble("x"), (float)point.getDouble("y"));
                            else path.lineTo((float)point.getDouble("x"), (float)point.getDouble("y"));
                        }
                        if (op.equals("polygon") || item.optBoolean("closed")) path.close();
                        if (op.equals("polyline")) {
                            paint.setStyle(Paint.Style.STROKE); paint.setStrokeWidth((float)item.getDouble("width"));
                            paint.setStrokeCap(Paint.Cap.ROUND); paint.setStrokeJoin(Paint.Join.ROUND);
                        } else path.setFillType(Path.FillType.EVEN_ODD);
                        canvas.drawPath(path, paint); break;
                    case "gradient":
                        String direction = item.getString("direction");
                        float endX = direction.equals("Vertical") ? rect.left : rect.right;
                        float endY = direction.equals("Horizontal") ? rect.top : rect.bottom;
                        paint.setShader(new LinearGradient(rect.left, rect.top, endX, endY,
                            (int)item.getLong("start"), (int)item.getLong("end"), Shader.TileMode.CLAMP));
                        float corner = (float)item.getDouble("radius"); canvas.drawRoundRect(rect, corner, corner, paint); break;
                    case "clip": canvas.save(); canvas.clipRect(rect); clipDepth++; break;
                    case "restore": if (clipDepth <= 0) throw new IllegalStateException("Unbalanced native clip"); canvas.restore(); clipDepth--; break;
                    case "text": drawText(canvas, item, rect); break;
                    case "image":
                        Bitmap bitmap = images.get(item.getString("id"));
                        if (bitmap == null) throw new IllegalStateException("Missing raster resource");
                        RectF source = rectangle(item.getJSONObject("source"));
                        paint.setFilterBitmap(item.optBoolean("smooth", true));
                        canvas.drawBitmap(bitmap, new Rect((int)source.left, (int)source.top, (int)source.right, (int)source.bottom), rect, paint);
                        break;
                    default: throw new IllegalStateException("Unsupported draw operation: " + op);
                }
            }
            if (clipDepth != 0) throw new IllegalStateException("Unbalanced native clip");
        } catch (Exception error) { Log.e("ZSUI", "Android Canvas rendering failed", error); }
        finally { canvas.restoreToCount(base); }
    }

    private static RectF rectangle(JSONObject value) throws JSONException {
        float x = (float)value.getDouble("x"), y = (float)value.getDouble("y");
        return new RectF(x, y, x + (float)value.getDouble("width"), y + (float)value.getDouble("height"));
    }

    private void retainImages() throws JSONException {
        HashSet<String> used = new HashSet<>();
        JSONArray commands = frame.getJSONArray("commands");
        for (int i = 0; i < commands.length(); i++) {
            JSONObject command = commands.getJSONObject(i);
            if (command.getString("op").equals("image")) used.add(command.getString("id"));
        }
        JSONArray resources = frame.optJSONArray("images");
        if (resources == null) return;
        long bytes = 0;
        for (Bitmap image : images.values()) bytes += image.getAllocationByteCount();
        for (int i = 0; i < resources.length(); i++) {
            JSONObject resource = resources.getJSONObject(i);
            String id = resource.getString("id");
            if (!used.contains(id) || images.containsKey(id)) continue;
            int width = resource.getInt("width"), height = resource.getInt("height");
            long count = (long)width * height;
            if (width <= 0 || height <= 0 || count > 2097152) throw new IllegalStateException("Raster resource budget exceeded");
            Iterator<Map.Entry<String,Bitmap>> old=images.entrySet().iterator();
            while((bytes+count*4>8388608||images.size()>=256)&&old.hasNext()){
                Map.Entry<String,Bitmap> entry=old.next();
                if(!used.contains(entry.getKey())){bytes-=entry.getValue().getAllocationByteCount();entry.getValue().recycle();old.remove();}
            }
            if(bytes+count*4>8388608||images.size()>=256)throw new IllegalStateException("Raster resource budget exceeded");
            bytes+=count*4;
            int[] pixels = new int[(int)count];
            if(resource.has("argb_base64")) {
                String encoded=resource.getString("argb_base64");
                if(encoded.length()!=((count*4+2)/3)*4)throw new IllegalStateException("Invalid compact raster length");
                byte[] raw=android.util.Base64.decode(encoded,android.util.Base64.NO_WRAP);
                if(raw.length!=count*4)throw new IllegalStateException("Invalid compact raster size");
                for(int pixel=0,offset=0;pixel<pixels.length;pixel++,offset+=4) pixels[pixel]=((raw[offset]&255)<<24)|((raw[offset+1]&255)<<16)|((raw[offset+2]&255)<<8)|(raw[offset+3]&255);
            } else {
                JSONArray values = resource.getJSONArray("argb");
                if (values.length() != count) throw new IllegalStateException("Invalid raster resource size");
                for (int pixel = 0; pixel < pixels.length; pixel++) pixels[pixel] = (int)values.getLong(pixel);
            }
            images.put(id, Bitmap.createBitmap(pixels, width, height, Bitmap.Config.ARGB_8888));
        }
        for(String id:used)images.get(id);
    }

    private static final class TextLayoutKey {
        final String text,weight,family,align;final int width,color,size;final boolean wrap,ellipsis;
        TextLayoutKey(JSONObject item,RectF rect)throws JSONException{
            text=item.getString("text");weight=item.getString("weight");family=item.isNull("family")?"sans-serif":item.optString("family","sans-serif");align=item.getString("align");
            width=Math.max(1,(int)rect.width());color=(int)item.getLong("color");size=Float.floatToIntBits((float)item.getDouble("size"));wrap=item.getString("wrap").equals("Word");ellipsis=item.optBoolean("ellipsis",true);
        }
        @Override public int hashCode(){return Objects.hash(text,weight,family,align,width,color,size,wrap,ellipsis);}
        @Override public boolean equals(Object other){if(!(other instanceof TextLayoutKey))return false;TextLayoutKey k=(TextLayoutKey)other;return width==k.width&&color==k.color&&size==k.size&&wrap==k.wrap&&ellipsis==k.ellipsis&&text.equals(k.text)&&weight.equals(k.weight)&&family.equals(k.family)&&align.equals(k.align);}
    }

    private void drawText(Canvas canvas, JSONObject item, RectF rect) throws JSONException {
        if (rect.width() <= 0 || rect.height() <= 0) return;
        TextLayoutKey key=new TextLayoutKey(item,rect);
        StaticLayout layout=key.text.length()<=4096?textLayouts.get(key):null;
        if(layout==null){
        textPaint.reset(); textPaint.setAntiAlias(true); textPaint.setSubpixelText(true);
        textPaint.setColor((int)item.getLong("color")); textPaint.setTextSize((float)item.getDouble("size"));
        String weight = item.getString("weight");
        boolean bold = weight.equals("Bold") || weight.equals("Semibold");
        String family = item.isNull("family") ? "sans-serif" : item.optString("family", "sans-serif");
        textPaint.setTypeface(Typeface.create(family, bold ? Typeface.BOLD : Typeface.NORMAL));
        Layout.Alignment alignment = Layout.Alignment.ALIGN_NORMAL;
        if (item.getString("align").equals("Center")) alignment = Layout.Alignment.ALIGN_CENTER;
        if (item.getString("align").equals("End")) alignment = Layout.Alignment.ALIGN_OPPOSITE;
        String value = item.getString("text");
        boolean wrap = item.getString("wrap").equals("Word");
        StaticLayout.Builder builder = StaticLayout.Builder.obtain(value, 0, value.length(), new TextPaint(textPaint), Math.max(1, (int)rect.width()))
            .setAlignment(alignment).setIncludePad(false).setMaxLines(wrap ? 100 : 1);
        if (item.optBoolean("ellipsis", true)) builder.setEllipsize(TextUtils.TruncateAt.END);
        layout = builder.build();
        if(key.text.length()<=4096)textLayouts.put(key,layout);
        }
        float y = rect.top;
        if (item.getString("vertical").equals("Center")) y += Math.max(0, (rect.height() - layout.getHeight()) / 2);
        if (item.getString("vertical").equals("End")) y += Math.max(0, rect.height() - layout.getHeight());
        int save = canvas.save();
        try {
            canvas.clipRect(rect);canvas.translate(rect.left,y);layout.draw(canvas);
            JSONObject highlight=item.optJSONObject("highlight");
            if(highlight!=null)drawTextHighlight(canvas,layout,highlight);
        } finally {canvas.restoreToCount(save);}
    }

    /** Optional presentation ranges. UTF-16 indices match Android's shaped text layout. */
    private void drawTextHighlight(Canvas canvas,StaticLayout layout,JSONObject value)throws JSONException {
        JSONArray ranges=value.getJSONArray("ranges");
        String text=layout.getText().toString();
        if(ranges.length()>256||text.length()>4096)throw new JSONException("Text highlight limit exceeded");
        Path combined=new Path(),selection=new Path(),part=new Path();RectF bounds=new RectF();
        for(int i=0;i<ranges.length();i++){
            JSONObject range=ranges.getJSONObject(i);int start=range.getInt("start"),end=range.getInt("end");double amount=range.getDouble("progress");
            if(start<0||end<=start||end>text.length()||!Double.isFinite(amount)||amount<0||amount>1
                ||start>0&&Character.isLowSurrogate(text.charAt(start))||end<text.length()&&Character.isLowSurrogate(text.charAt(end)))throw new JSONException("Invalid text highlight");
            if(amount==0)continue;
            selection.reset();layout.getSelectionPath(start,end,selection);
            if(amount<1){
                selection.computeBounds(bounds,true);float width=(float)(bounds.width()*amount);
                if(layout.isRtlCharAt(start))bounds.left=bounds.right-width;else bounds.right=bounds.left+width;
                part.reset();part.addRect(bounds,Path.Direction.CW);selection.op(part,Path.Op.INTERSECT);
            }
            combined.addPath(selection);
        }
        if(combined.isEmpty())return;
        int save=canvas.save(),previous=layout.getPaint().getColor();
        try{canvas.clipPath(combined);layout.getPaint().setColor((int)value.getLong("color"));layout.draw(canvas);}
        finally{layout.getPaint().setColor(previous);canvas.restoreToCount(save);}
    }
}
