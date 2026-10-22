package io.zsui.android;

import android.view.MotionEvent;
import android.view.VelocityTracker;
import android.view.ViewConfiguration;
import android.widget.OverScroller;
import org.json.JSONArray;
import org.json.JSONObject;
import org.json.JSONException;
import java.util.ArrayList;

/** Per-View kinetic input. No application state, navigation or global registry. */
final class ZsuiKineticScroller {
    private final ZsuiView view;
    private final OverScroller scroller;
    private final int slop,minimum,maximum;
    private final ArrayList<Area> areas=new ArrayList<>();
    private VelocityTracker velocity;
    private String context="",areaId;
    private float downX,downY;
    private boolean stopOnlyTap;
    private int previousY,width,height;

    private static final class Area {
        String id;float x,y,w,h,offset,max;
        boolean contains(float px,float py){return px>=x&&px<x+w&&py>=y&&py<y+h;}
        boolean sameGeometry(Area b){return b!=null&&x==b.x&&y==b.y&&w==b.w&&h==b.h;}
    }

    ZsuiKineticScroller(ZsuiView view){
        this.view=view;scroller=new OverScroller(view.getContext());
        ViewConfiguration c=ViewConfiguration.get(view.getContext());
        slop=c.getScaledTouchSlop();minimum=c.getScaledMinimumFlingVelocity();maximum=c.getScaledMaximumFlingVelocity();
    }

    void apply(JSONObject value)throws JSONException {
        String next=value==null?"":value.optString("context");
        Area previous=find(areaId);ArrayList<Area> updated=new ArrayList<>();
        JSONArray list=value==null?null:value.optJSONArray("scrolls");
        if(list!=null){
            if(list.length()>32)throw new JSONException("Too many scroll regions");
            for(int i=0;i<list.length();i++){
                JSONObject item=list.getJSONObject(i);JSONArray rect=item.getJSONArray("rect");
                if(rect.length()!=4)throw new JSONException("Invalid scroll rectangle");
                Area a=new Area();a.id=item.getString("id");a.x=(float)rect.getDouble(0);a.y=(float)rect.getDouble(1);a.w=(float)rect.getDouble(2);a.h=(float)rect.getDouble(3);
                a.offset=(float)item.getDouble("offset");a.max=(float)item.getDouble("max_offset");
                for(float n:new float[]{a.x,a.y,a.w,a.h,a.offset,a.max})if(!Float.isFinite(n)||Math.abs(n)>80000000)throw new JSONException("Invalid scroll range");
                if(a.w<0||a.h<0||a.offset<0||a.max<a.offset)throw new JSONException("Invalid scroll bounds");
                updated.add(a);
            }
        }
        areas.clear();areas.addAll(updated);
        if(!next.equals(context)||previous!=null&&!previous.sameGeometry(find(areaId)))stop();
        context=next;
    }

    private Area find(String id){if(id!=null)for(Area area:areas)if(id.equals(area.id))return area;return null;}

    /** Returns true for the tap that only stops a moving viewport. */
    boolean observe(MotionEvent event){
        int action=event.getActionMasked();
        if(action==MotionEvent.ACTION_DOWN){
            stopOnlyTap=!scroller.isFinished();stop();downX=event.getX();downY=event.getY();
            for(int i=areas.size()-1;i>=0;i--)if(areas.get(i).max>0&&areas.get(i).contains(downX,downY)){areaId=areas.get(i).id;break;}
            velocity=VelocityTracker.obtain();velocity.addMovement(event);return false;
        }
        if(action==MotionEvent.ACTION_POINTER_DOWN||action==MotionEvent.ACTION_CANCEL){stop();return false;}
        if(velocity==null)return false;
        velocity.addMovement(event);
        if(action!=MotionEvent.ACTION_UP)return false;
        float dx=event.getX()-downX,dy=event.getY()-downY;
        velocity.computeCurrentVelocity(1000,maximum);float speed=velocity.getYVelocity();velocity.recycle();velocity=null;
        if(stopOnlyTap&&Math.abs(dx)<slop&&Math.abs(dy)<slop)return true;
        Area area=find(areaId);
        if(area==null||Math.abs(dy)<slop*2||Math.abs(dy)<Math.abs(dx)*1.4f||Math.abs(speed)<minimum)return false;
        previousY=Math.round(area.offset);width=view.getWidth();height=view.getHeight();
        scroller.fling(0,previousY,0,Math.round(-speed),0,0,0,Math.round(area.max));
        view.postOnAnimation(tick);return false;
    }

    private final Runnable tick=new Runnable(){public void run(){
        Area area=find(areaId);
        if(area==null||view.getWidth()!=width||view.getHeight()!=height||!view.hasWindowFocus()){stop();return;}
        if(!scroller.computeScrollOffset())return;
        int y=scroller.getCurrY(),delta=y-previousY;previousY=y;
        if(delta!=0){
            float before=area.offset;
            view.submit(ZsuiView.event("scroll","x",downX,"y",downY,"delta_y",delta));
            Area after=find(areaId);
            if(after==null||Math.abs(after.offset-before)<0.5f){stop();return;}
        }
        if(!scroller.isFinished())view.postOnAnimation(this);
    }};

    void stop(){view.removeCallbacks(tick);scroller.abortAnimation();areaId=null;if(velocity!=null){velocity.recycle();velocity=null;}}
}
